import { join } from 'node:path'
import { type IpcMainInvokeEvent, BrowserWindow, app, ipcMain, session } from 'electron'
import {
  type ApiResult,
  type EngineStatus,
  type ServerMessage,
  GROUND,
  INK,
  IPC,
  MOCK_ARG,
  TITLE_BAR_HEIGHT,
} from '@td/shared'
import { RefusedRequest, sendRequest, validateRequest, validateTopic } from './bridge'
import { createEngine } from './engine'
import { createQuit } from './quit'
import { Sidecar, sidecarBin } from './sidecar'
import { EngineSocket } from './socket'
import { spawnWatchdog } from './watchdog'

// Sized so the Lab's 680 px reading column, its margins and the 12 px ground strip fit at the
// minimum, and the verdict and composer both stay in view on a 768 px tall laptop screen.
const MIN_WIDTH = 900
const MIN_HEIGHT = 600

const log = (line: string) => console.log(`[td-server] ${line}`)

let window: BrowserWindow | null = null
// Aborted when the app quits, so no request to the engine is left open.
const requests = new AbortController()

const socket = new EngineSocket((message: ServerMessage) => {
  window?.webContents.send(IPC.message, message)
}, log)

const quit = createQuit({
  stopEngine: () => engine.stop(),
  killEngine: () => engine.kill(),
  closeConnections: () => {
    engine.disconnect()
    requests.abort()
  },
  exit: (code) => app.exit(code),
  // shortcut: the watchdog kills only the main process; Chromium's helpers exit when it goes.
  // Revisit if a real launch ever shows helpers outliving it.
  armWatchdog: (afterMs) => spawnWatchdog(process.pid, afterMs),
  log,
})

const engine = createEngine({
  spawn: () => new Sidecar({ bin: sidecarBin(process.env, app.getAppPath(), app.isPackaged), env: process.env, log }),
  openSocket: (connection) => socket.open(connection),
  closeSocket: () => socket.close(),
  onStatus: (status: EngineStatus) => window?.webContents.send(IPC.statusChanged, status),
  quit,
  log,
})

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
    const connection = engine.connection
    if (!connection || quit.quitting) {
      const status = engine.status
      const why = status.state === 'failed' ? status.message : 'the engine is still starting'
      return { ok: false, status: 0, error: { code: 'engine_unavailable', message: why } }
    }
    return sendRequest(connection, request, requests.signal)
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
    return engine.status
  })
  ipcMain.handle(IPC.restartEngine, async (event, ...args: unknown[]) => {
    checkSender(event)
    if (args.length > 0) throw new Error('restartEngine takes no arguments')
    await engine.restart()
  })
}

function createWindow(): void {
  const mock = process.env.TD_MOCK === '1'
  const win = new BrowserWindow({
    width: 1280,
    height: 800,
    minWidth: MIN_WIDTH,
    minHeight: MIN_HEIGHT,
    show: false,
    title: 'Trading Desk',
    backgroundColor: GROUND,
    titleBarStyle: 'hidden',
    // The overlay sits on the ground strip, so it takes the ground's colour.
    titleBarOverlay: { color: GROUND, symbolColor: INK, height: TITLE_BAR_HEIGHT },
    webPreferences: {
      preload: join(__dirname, '../preload/index.js'),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      additionalArguments: mock ? [MOCK_ARG] : [],
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

// One app at a time. A second instance would start a second engine on the same data folder and
// share Chromium's profile folder with the first, whose locked files can stall its shutdown.
if (!app.requestSingleInstanceLock()) {
  app.exit(0)
} else {
  app.on('second-instance', () => {
    // shortcut: a relaunch is dropped while the engine stops, for up to 13 s (QUIT_TIMINGS); the
    // user has to launch again after. Upgrade to relaunching once this process exits if it bites.
    if (quit.quitting) {
      log('a second launch arrived while the app was stopping; ignored')
      return
    }
    if (!window) return
    if (window.isMinimized()) window.restore()
    window.focus()
  })

  app.whenReady().then(() => {
    if (quit.quitting) return
    session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false))
    registerIpc()
    createWindow()
    void engine.start()
  })

  // One window: closing it quits the app, and quitting stops the engine first (quit.ts).
  app.on('window-all-closed', () => app.quit())
  app.on('before-quit', (event) => quit.onBeforeQuit(event))

  // Last resort if the process exits without going through before-quit.
  process.on('exit', () => engine.kill())
}
