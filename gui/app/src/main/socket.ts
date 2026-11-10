// The app's one WebSocket to the engine, held in main so the token stays here. The token goes in
// the `td-token.<token>` subprotocol (crates/td-server/src/api/auth.rs). Topics the renderer asked
// for are re-sent whenever the socket (re)opens.

import type { ClientMessage, ServerMessage } from '@td/shared'
import type { Connection } from './bridge'

const RECONNECT_MS = 1_000

export class EngineSocket {
  private readonly topics = new Set<string>()
  private socket: WebSocket | null = null
  private connection: Connection | null = null
  private retry: NodeJS.Timeout | undefined

  constructor(
    private readonly onMessage: (message: ServerMessage) => void,
    private readonly log: (line: string) => void,
  ) {}

  /** Connects to a running engine, and reconnects after a drop until `close`. */
  open(connection: Connection): void {
    this.connection = connection
    this.connect()
  }

  /** Disconnects and stops reconnecting. The topics are kept for the next `open`. */
  close(): void {
    this.connection = null
    clearTimeout(this.retry)
    this.socket?.close()
    this.socket = null
  }

  subscribe(topic: string): void {
    if (this.topics.has(topic)) return
    this.topics.add(topic)
    this.send({ type: 'subscribe', topic })
  }

  unsubscribe(topic: string): void {
    if (!this.topics.delete(topic)) return
    this.send({ type: 'unsubscribe', topic })
  }

  private connect(): void {
    const connection = this.connection
    if (!connection) return
    const socket = new WebSocket(`ws://127.0.0.1:${connection.port}/ws`, [`td-token.${connection.token}`])
    this.socket = socket
    socket.addEventListener('open', () => {
      for (const topic of this.topics) this.send({ type: 'subscribe', topic })
    })
    socket.addEventListener('message', (event) => {
      if (typeof event.data !== 'string') return
      let message: unknown
      try {
        message = JSON.parse(event.data)
      } catch {
        this.log('engine WebSocket sent a message that is not JSON')
        return
      }
      if (typeof message === 'object' && message !== null && typeof (message as { type?: unknown }).type === 'string') {
        this.onMessage(message as ServerMessage)
      }
    })
    socket.addEventListener('close', (event) => {
      if (this.socket !== socket) return
      this.socket = null
      if (!this.connection) return
      this.log(`engine WebSocket closed (${event.code}); reconnecting`)
      this.retry = setTimeout(() => this.connect(), RECONNECT_MS)
    })
  }

  private send(message: ClientMessage): void {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message))
  }
}
