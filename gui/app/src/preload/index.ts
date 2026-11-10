import { type IpcRendererEvent, contextBridge, ipcRenderer } from 'electron'
import { type EngineStatus, type ServerMessage, type TdBridge, IPC } from '@td/shared'

function listen<T>(channel: string, listener: (value: T) => void): () => void {
  const handler = (_event: IpcRendererEvent, value: T) => listener(value)
  ipcRenderer.on(channel, handler)
  return () => {
    ipcRenderer.removeListener(channel, handler)
  }
}

const td: TdBridge = {
  request: (request) => ipcRenderer.invoke(IPC.request, request),
  subscribe: (topic) => ipcRenderer.invoke(IPC.subscribe, topic),
  unsubscribe: (topic) => ipcRenderer.invoke(IPC.unsubscribe, topic),
  onMessage: (listener) => listen<ServerMessage>(IPC.message, listener),
  status: () => ipcRenderer.invoke(IPC.status),
  onStatus: (listener) => listen<EngineStatus>(IPC.statusChanged, listener),
}

contextBridge.exposeInMainWorld('td', td)
