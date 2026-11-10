// Test-only stand-in for the preload's `window.td`, injected by shots.mjs and the UI gates (gui/e2e)
// before the page loads.
// It implements the TdBridge contract (gui/shared/src/bridge.ts) with a scripted engine status and
// answers requests from the mock fixtures. Never part of the app bundle.
//
// `config`: { engine: 'starting' | 'failed' | 'running', mock: boolean, hangStrategies: boolean,
//             strategies: StrategyInfo[] }

function installBridgeStub(config) {
  const listeners = new Set()
  let status =
    config.engine === 'failed'
      ? { state: 'failed', message: 'td-server exited before its handshake (exit code 3): cannot open the data directory' }
      : config.engine === 'starting'
        ? { state: 'connecting' }
        : { state: 'running' }

  const ok = (body) => ({ ok: true, status: 200, body })

  window.td = {
    mock: config.mock,
    async request(request) {
      if (request.method === 'GET' && request.path === '/health') return ok({ version: '0.1.0' })
      if (request.method === 'GET' && request.path === '/strategies') {
        if (config.hangStrategies) return new Promise(() => {})
        return ok(config.strategies)
      }
      return { ok: false, status: 404, error: { code: 'not_found', message: `${request.path} is not stubbed` } }
    },
    async subscribe() {},
    async unsubscribe() {},
    onMessage() {
      return () => {}
    },
    async status() {
      return status
    },
    onStatus(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    // Retry restarts the engine: it reports connecting, then running a moment later.
    async restartEngine() {
      if (status.state !== 'failed') return
      const set = (next) => {
        status = next
        for (const listener of listeners) listener(status)
      }
      set({ state: 'connecting' })
      setTimeout(() => set({ state: 'running' }), 300)
    },
  }
}
