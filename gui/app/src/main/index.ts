import { join } from 'node:path'
import { type IpcMainInvokeEvent, BrowserWindow, app, ipcMain, session } from 'electron'
import { type ApiResult, type EngineStatus, type ServerMessage, IPC } from '@td/shared'
import { type Connection, RefusedRequest, sendRequest, validateRequest, validateTopic } from './bridge'
import { Sidecar, sidecarBin } from './sidecar'
import { EngineSocket } from './socket'

const TITLE_BAR_HEIGHT = 36

const log = (line: string) => console.log(`[td-server] ${line}`)

let window: BrowserWindow | null = null
let sidecar: Sidecar | null = null
let connection: Connection | null = null
let status: EngineStatus = { state: 'connecting' }
let quitting = false
let stopped = false

const socket = new EngineSocket((message: ServerMessage) => {
  window?.webContents.send(IPC.message, message)
}, log)

function setStatus(next: EngineStatus): void {
  status = next
  window?.webContents.send(IPC.statusChanged, next)
}

async function startEngine(): Promise<void> {
  const bin = sidecarBin(process.env, app.getAppPath(), app.isPackaged)
  const engine = new Sidecar({ bin, env: process.env, log })
  sidecar = engine
  void engine.exited.then(({ code, signal }) => {
    connection = null
    socket.close()
    if (!quitting && status.state === 'running') {
      setStatus({ state: 'failed', message: `the engine stopped (${signal ? `signal ${signal}` : `exit code ${code}`})` })
    }
  })
  try {
    connection = await engine.ready
    socket.open(connection)
    setStatus({ state: 'running' })
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e)
    log(message)
    setStatus({ state: 'failed', message })
  }
}

/** Only the app's own window, from its top frame, may use the bridge. */
function checkSender(event: IpcMainInvokeEvent): void {
  if (!window || event.sender !== window.webContents || event.senderFrame !== window.webContents.mainFrame) {
    throw new Error('IPC from an unknown sender')
  }
}

function registerIpc(): void {
  ipcMain.handle(IPC.request, async (event, raw: unknown): Promise<ApiResult> => {
    checkSender(event)
    let request
    try {
      request = validateRequest(raw)
    } catch (e) {
      if (!(e instanceof RefusedRequest)) throw e
      log(`refused a renderer request: ${e.message}`)
      return { ok: false, status: 0, error: { code: 'refused', message: e.message } }
    }
    if (!connection) {
      const why = status.state === 'failed' ? status.message : 'the engine is still starting'
      return { ok: false, status: 0, error: { code: 'engine_unavailable', message: why } }
    }
    return sendRequest(connection, request)
  })
  ipcMain.handle(IPC.subscribe, (event, raw: unknown) => {
    checkSender(event)
    socket.subscribe(validateTopic(raw))
  })
  ipcMain.handle(IPC.unsubscribe, (event, raw: unknown) => {
    checkSender(event)
    socket.unsubscribe(validateTopic(raw))
  })
  ipcMain.handle(IPC.status, (event) => {
    checkSender(event)
    return status
  })
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1280,
    height: 800,
    show: false,
    title: 'Trading Desk',
    titleBarStyle: 'hidden',
    titleBarOverlay: { height: TITLE_BAR_HEIGHT },
    webPreferences: {
      preload: join(__dirname, '../preload/index.js'),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  })
  window = win
  win.once('ready-to-show', () => win.show())
  win.on('closed', () => {
    window = null
  })
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }))
  win.webContents.on('will-navigate', (event, url) => {
    if (url !== win.webContents.getURL()) event.preventDefault()
  })
  const devUrl = process.env.ELECTRON_RENDERER_URL
  if (!app.isPackaged && devUrl) void win.loadURL(devUrl)
  else void win.loadFile(join(__dirname, '../renderer/index.html'))
}

app.whenReady().then(() => {
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false))
  registerIpc()
  createWindow()
  void startEngine()
})

// One window: closing it quits the app, and quitting stops the engine first.
app.on('window-all-closed', () => app.quit())

app.on('before-quit', (event) => {
  if (stopped) return
  event.preventDefault()
  if (quitting) return
  quitting = true
  socket.close()
  const engine = sidecar
  void (engine ? engine.stop() : Promise.resolve('exited'))
    .then((result) => log(`stopped (${result})`))
    .finally(() => {
      stopped = true
      app.quit()
    })
})

// Last resort if the process exits without going through before-quit.
process.on('exit', () => sidecar?.kill())
