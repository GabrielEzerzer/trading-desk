// Test-only stand-in for the preload's `window.td`, injected by shots.mjs before the page loads.
// It implements the TdBridge contract (gui/shared/src/bridge.ts) with a scripted engine status and
// answers requests from the mock fixtures. Never part of the app bundle.
//
// `config`: { engine: 'starting' | 'failed' | 'running', mock: boolean, hangStrategies: boolean,
//             strategies: StrategyInfo[], instruments: Instrument[], hangInstruments: boolean, failInstruments: boolean,
//             candles: { start: Candle, everyMs: number } | null, session: SessionState | null }
//
// For Trading in engine mode it answers GET /instruments from the fixtures, GET /sessions and
// GET /sessions/{id} from `config.session`, and on a `candles:` subscription emits a closed bar
// every `config.candles.everyMs`, walking from `config.candles.start`.

function installBridgeStub(config) {
  const listeners = new Set()
  let status =
    config.engine === 'failed'
      ? { state: 'failed', message: 'td-server exited before its handshake (exit code 3): cannot open the data directory' }
      : config.engine === 'starting'
        ? { state: 'connecting' }
        : { state: 'running' }

  const ok = (body) => ({ ok: true, status: 200, body })
  const messageListeners = new Set()
  const emit = (message) => {
    for (const listener of messageListeners) listener(message)
  }
  const timers = new Map()
  const summary = (state) => ({
    id: state.id,
    strategy: state.config.strategy,
    instrument: state.config.instrument.id,
    timeframe: state.config.timeframe,
    status: state.status,
    equity: state.equity,
  })

  function emitCandles(topic) {
    if (!config.candles || timers.has(topic)) return
    const step = { '1m': 60_000, '5m': 300_000, '15m': 900_000, '1h': 3_600_000, '4h': 14_400_000 }[topic.split(':')[4]] ?? 60_000
    let bar = { ...config.candles.start }
    let n = 0
    timers.set(
      topic,
      setInterval(() => {
        // A small deterministic walk, enough to see the chart fill.
        const drift = Math.sin(n / 3) * bar.close * 0.0012 + Math.cos(n / 7) * bar.close * 0.0006
        const open = bar.close
        const close = open + drift
        bar = {
          open_time: bar.open_time + step,
          open,
          high: Math.max(open, close) + Math.abs(drift) * 0.6,
          low: Math.min(open, close) - Math.abs(drift) * 0.5,
          close,
          volume: 12 + (n % 5),
          quote_volume: (12 + (n % 5)) * close,
        }
        n += 1
        emit({ type: 'candle', topic, candle: bar })
      }, config.candles.everyMs),
    )
  }

  window.td = {
    mock: config.mock,
    async request(request) {
      if (request.method === 'GET' && request.path === '/health') return ok({ version: '0.1.0' })
      if (request.method === 'GET' && request.path === '/strategies') {
        if (config.hangStrategies) return new Promise(() => {})
        return ok(config.strategies)
      }
      if (request.method === 'GET' && request.path === '/instruments') {
        if (config.hangInstruments) return new Promise(() => {})
        if (config.failInstruments) {
          return { ok: false, status: 502, error: { code: 'upstream', message: `${request.query?.venue} did not answer: connection timed out` } }
        }
        const { venue, kind } = request.query ?? {}
        const instruments = (config.instruments ?? []).filter((i) => i.id.venue === venue && i.id.kind === kind)
        return ok({ instruments, rejected: [] })
      }
      if (request.method === 'GET' && request.path === '/sessions') return ok(config.session ? [summary(config.session)] : [])
      if (request.method === 'GET' && config.session && request.path === `/sessions/${config.session.id}`) return ok(config.session)
      return { ok: false, status: 404, error: { code: 'not_found', message: `${request.path} is not stubbed` } }
    },
    async subscribe(topic) {
      if (topic.startsWith('candles:')) emitCandles(topic)
      if (config.session && topic === `session:${config.session.id}`) {
        setTimeout(() => emit({ type: 'session_state', session: config.session }), 0)
      }
    },
    async unsubscribe(topic) {
      clearInterval(timers.get(topic))
      timers.delete(topic)
    },
    onMessage(listener) {
      messageListeners.add(listener)
      return () => messageListeners.delete(listener)
    },
    async status() {
      return status
    },
    onStatus(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    async restartEngine() {
      if (status.state !== 'failed') return
      status = { state: 'connecting' }
      for (const listener of listeners) listener(status)
    },
  }
}
