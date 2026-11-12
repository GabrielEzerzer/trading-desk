// Test-only stand-in for the preload's `window.td`, injected by shots.mjs and the UI gates (gui/e2e)
// before the page loads.
// It implements the TdBridge contract (gui/shared/src/bridge.ts) with a scripted engine status, and
// answers the engine's routes from the fixtures in app/mock/engine.json: the strategy list, saved
// runs, stored-data coverage, instrument rules, and backtest and download jobs whose changes arrive
// on the `jobs` topic like the engine's. Never part of the app bundle.
//
// `config`: { engine: 'starting' | 'failed' | 'running', mock: boolean, hangStrategies: boolean,
//             engineData: engine.json, noData?: boolean,
//             backtest?: 'ok' | 'data_missing' | 'too_many_runs' | 'fails', stepMs?: number,
//             instruments?: Instrument[], hangInstruments?: boolean, failInstruments?: boolean,
//             candles?: { start: Candle, everyMs: number } | null, session?: SessionState | null,
//             failSessions?: number,
//             history?: Record<string, number | 'hang' | 'fail'> }
//
// Every request is recorded in `window.__tdCalls`, and the topics subscribed now in
// `window.__stubSubscribed`, so a flow can check what the app sent.
//
// For Trading in engine mode it answers GET /instruments from `config.instruments` (else the
// engine.json fixtures), GET /sessions and GET /sessions/{id} from `config.session`, and on a
// `candles:` subscription emits a closed bar every `config.candles.everyMs`, walking from
// `config.candles.start`, with the next bar forming half way between; `window.__stubCandles` counts
// the closed ones and `window.__stubFormed` the forming ones, so a state can wait for the chart to
// fill. GET /candles answers with `limit` bars
// ending at `config.candles.start` (none without `config.candles`). `config.history` keys an
// instrument (`venue:symbol:kind`) or `*` to how its history answers: after that many
// milliseconds, never (`hang`), or with the engine's 502 (`fail`); without an entry it answers at
// once. GET /sessions fails the first `config.failSessions` times. POST /sessions answers after
// 300 ms with a new warming-up session, which GET /sessions then lists; `window.__stubStarts`
// counts the requests. POST /sessions/{id}/orders adds an open manual order to that session and
// POST /sessions/{id}/orders/{n}/cancel cancels one, each reported on its `session:` topic like the
// engine's; neither fills. Like the engine, an order before the session's first live bar gets 409
// `not_live`; `window.__stubGoLive()` makes the session live and reports it on its topic.

function installBridgeStub(config) {
  const listeners = new Set()
  let status =
    config.engine === 'failed'
      ? { state: 'failed', message: 'td-server exited before its handshake (exit code 3): cannot open the data directory' }
      : config.engine === 'starting'
        ? { state: 'connecting' }
        : { state: 'running' }

  const data = config.engineData
  const stepMs = config.stepMs ?? 700
  const key = (id) => `${id.venue}:${id.symbol}:${id.kind}`
  // Instruments with candles stored; a download adds its instrument.
  const stored = new Set(config.noData ? [] : Object.keys(data.coverage))
  let backtest = config.backtest ?? 'ok'
  const jobs = new Map()
  let nextJob = 1
  const messageListeners = new Set()
  let subscribed = false
  const calls = []
  window.__tdCalls = calls
  // A flow can change how GET /candles answers from here on.
  const history = { ...config.history }
  window.__stubHistory = history

  const ok = (body) => ({ ok: true, status: 200, body })
  const fail = (status, code, message) => ({ ok: false, status, error: { code, message } })

  const emit = (message) => {
    for (const listener of messageListeners) listener(message)
  }
  const emitJob = (job) => {
    if (subscribed) emit({ type: 'job', job: structuredClone(job) })
  }

  /**
   * Starts a job that reports `steps` one `stepMs` apart, then ends as `finish` says. Like the
   * engine's checkpoints, a cancel stops a running job before its next step, but not once its last
   * step has started: then the job still ends as `finish` says.
   */
  function startJob(kind, request, steps, finish) {
    const job = {
      id: nextJob++,
      kind,
      status: 'queued',
      progress: 0,
      message: 'queued',
      request: { kind, ...request },
      result: null,
      error: null,
      cancel_requested: false,
      created_at: Date.now(),
      started_at: null,
      finished_at: null,
    }
    jobs.set(job.id, job)
    // Like the engine, the queued job is published before the route answers.
    emitJob(job)
    let i = 0
    const tick = () => {
      // A queued job is cancelled at once, by the cancel route.
      if (job.status === 'cancelled') return
      if (job.cancel_requested && i < steps.length) {
        Object.assign(job, { status: 'cancelled', message: 'cancelled', finished_at: Date.now() })
        emitJob(job)
        return
      }
      const step = steps[i++]
      if (step) {
        Object.assign(job, { status: 'running', started_at: job.started_at ?? Date.now() }, step)
        emitJob(job)
        setTimeout(tick, stepMs)
        return
      }
      Object.assign(job, { finished_at: Date.now() }, finish())
      emitJob(job)
    }
    setTimeout(tick, 0)
    return { ok: true, status: 202, body: { job_id: job.id } }
  }

  function startBacktest(body) {
    if (backtest === 'too_many_runs') {
      // The engine's text (td-server jobs.rs, MAX_RUN_JOBS = 8).
      return fail(429, 'too_many_runs', '8 runs are already queued or running, the most this server takes; wait for one to finish or cancel one')
    }
    if (backtest === 'data_missing') {
      return fail(422, 'data_missing', `the store has no candles for ${key(body.instrument.id)} from 2024-03-04 00:00 to 2024-03-12 00:00 UTC; download them first`)
    }
    const steps = [
      { progress: 0, message: 'loading the data' },
      { progress: 0, message: 'running the backtest' },
      { progress: 1, message: 'saving the result' },
    ]
    return startJob('backtest', body, steps, () => {
      if (backtest === 'fails') {
        return { status: 'failed', error: { code: 'strategy_error', message: 'the strategy refused its parameters: slow must be above fast' } }
      }
      const detail = data.details[data.runResult]
      const { metrics, config: run } = detail.run.result
      const summary = {
        id: detail.id,
        created_at: Number(detail.id.slice(0, 13)),
        kind: 'backtest',
        strategy: run.strategy,
        instrument: run.instrument.id,
        timeframe: run.timeframe,
        from: run.from,
        to: run.to,
        total_return: metrics.total_return,
        cagr: metrics.cagr,
        sharpe: metrics.sharpe,
        max_drawdown: metrics.max_drawdown,
        trades: metrics.trades,
      }
      return { status: 'done', progress: 1, result: { kind: 'backtest', run_id: detail.id, summary, dropped_buckets: 0 } }
    })
  }

  function startDownload(body) {
    const steps = [0.25, 0.5, 0.75, 1].map((progress, i) => ({ progress, message: `${key(body.instrument)} month ${i + 1}` }))
    return startJob('download', body, steps, () => {
      stored.add(key(body.instrument))
      // The download filled what a refused run was missing.
      if (backtest === 'data_missing') backtest = 'ok'
      return { status: 'done', progress: 1, result: { kind: 'download', candles: 1000, months: [], missing_files: [], gaps: [], funding_rates: null } }
    })
  }

  function route({ method, path, query, body }) {
    if (method === 'GET' && path === '/health') return ok({ version: '0.1.0' })
    if (method === 'GET' && path === '/strategies') return ok(data.strategies)
    if (method === 'GET' && path === '/runs') return ok(data.runs)
    let m = /^\/runs\/([\w-]+)$/.exec(path)
    if (method === 'GET' && m && m[1] !== 'backtest') {
      const detail = data.details[m[1]]
      return detail ? ok({ ...detail, check: { data_matches: true, engine_matches: true } }) : fail(404, 'not_found', `no run ${m[1]}`)
    }
    if (method === 'GET' && path === '/data/coverage') {
      const id = query.instrument
      // A downloaded instrument without coverage of its own reads like the first stored one.
      const months = stored.has(id) ? (data.coverage[id] ?? Object.values(data.coverage)[0]) : []
      const [venue, symbol, kind] = id.split(':')
      // A perpetual reports its stored funding rates, one every 8 hours over the stored range; spot reports null.
      let funding = null
      if (kind === 'perpetual') {
        const step = 8 * 3_600_000
        const first = months.length ? Math.ceil(months[0].first / step) * step : null
        const last = months.length ? Math.floor(months.at(-1).last / step) * step : null
        funding = first === null ? { rates: 0, first: null, last: null } : { rates: (last - first) / step + 1, first, last }
      }
      return ok({ instrument: { venue, symbol, kind }, months, funding })
    }
    if (method === 'GET' && path === '/instruments') {
      const instruments = (config.instruments ?? data.instruments).filter((i) => i.id.venue === query.venue && i.id.kind === query.kind)
      return ok({ instruments, rejected: [] })
    }
    if (method === 'POST' && path === '/runs/backtest') return startBacktest(body)
    if (method === 'POST' && path === '/data/download') return startDownload(body)
    m = /^\/jobs\/(\d+)(?:\/(cancel|result))?$/.exec(path)
    const job = m && jobs.get(Number(m[1]))
    if (m && !job) return fail(404, 'not_found', `no job ${m[1]}`)
    if (method === 'POST' && m?.[2] === 'cancel') {
      if (job.status !== 'queued' && job.status !== 'running') return fail(409, 'job_finished', `job ${job.id} has already finished`)
      // A queued job is cancelled at once; a running one only asks, and ends at its next step.
      job.cancel_requested = true
      if (job.status === 'queued') Object.assign(job, { status: 'cancelled', message: 'cancelled', finished_at: Date.now() })
      emitJob(job)
      return { ok: true, status: 202, body: structuredClone(job) }
    }
    if (method === 'GET' && m && !m[2]) return ok(structuredClone(job))
    if (method === 'GET' && m?.[2] === 'result') {
      if (job.status !== 'done') return fail(409, 'job_not_done', `job ${job.id} is ${job.status}, not done yet`)
      return ok(data.details[job.result.run_id])
    }
    return fail(404, 'not_found', `${method} ${path} is not stubbed`)
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

  const STEPS = { '1m': 60_000, '5m': 300_000, '15m': 900_000, '30m': 1_800_000, '1h': 3_600_000, '4h': 14_400_000 }

  /** `limit` bars ending at `config.candles.start`, a small deterministic walk back from it. */
  function historyBars(timeframe, limit) {
    if (!config.candles) return []
    const step = STEPS[timeframe] ?? 60_000
    const start = config.candles.start
    const bars = [{ ...start }]
    let close = start.open
    for (let k = 1; k < limit; k++) {
      const drift = Math.sin(k / 5) * close * 0.0015 + Math.cos(k / 11) * close * 0.0008
      const open = close - drift
      const volume = 12 + (k % 5)
      bars.unshift({
        open_time: start.open_time - k * step,
        open,
        high: Math.max(open, close) + Math.abs(drift) * 0.6,
        low: Math.min(open, close) - Math.abs(drift) * 0.5,
        close,
        volume,
        quote_volume: volume * close,
      })
      close = open
    }
    return bars
  }

  async function candles(query) {
    const how = history[query.instrument] ?? history['*']
    if (how === 'hang') return new Promise(() => {})
    if (typeof how === 'number') await new Promise((resolve) => setTimeout(resolve, how))
    if (how === 'fail') {
      // The engine's words for a venue that did not answer (td-server candles.rs, venue_error).
      return fail(502, 'venue', `fetching ${query.instrument} ${query.timeframe} bars from the venue failed: connection timed out`)
    }
    const [venue, symbol, kind] = query.instrument.split(':')
    const bars = historyBars(query.timeframe, Number(query.limit))
    return ok({
      instrument: { venue, symbol, kind },
      timeframe: query.timeframe,
      from: bars[0]?.open_time ?? 0,
      to: (bars.at(-1)?.open_time ?? 0) + (STEPS[query.timeframe] ?? 60_000),
      truncated: false,
      candles: bars,
      from_venue: [],
      missing: [],
    })
  }

  function emitCandles(topic) {
    if (!config.candles || timers.has(topic)) return
    const step = STEPS[topic.split(':')[4]] ?? 60_000
    let bar = { ...config.candles.start }
    let n = 0
    const forming = []
    const next = () => {
      // A small deterministic walk, enough to see the chart fill.
      const drift = Math.sin(n / 3) * bar.close * 0.0012 + Math.cos(n / 7) * bar.close * 0.0006
      const open = bar.close
      const close = open + drift
      return {
        open_time: bar.open_time + step,
        open,
        high: Math.max(open, close) + Math.abs(drift) * 0.6,
        low: Math.min(open, close) - Math.abs(drift) * 0.5,
        close,
        volume: 12 + (n % 5),
        quote_volume: (12 + (n % 5)) * close,
      }
    }
    // Half way to each close, the bar so far: its open, half its move and half its volume.
    const showForming = () => {
      const whole = next()
      const close = whole.open + (whole.close - whole.open) / 2
      const volume = whole.volume / 2
      const candle = { ...whole, high: Math.max(whole.open, close), low: Math.min(whole.open, close), close, volume, quote_volume: volume * close }
      window.__stubFormed = (window.__stubFormed ?? 0) + 1
      emit({ type: 'candle_forming', topic, candle })
    }
    forming.push(setTimeout(showForming, config.candles.everyMs / 2))
    const interval = setInterval(() => {
      bar = next()
      n += 1
      window.__stubCandles = (window.__stubCandles ?? 0) + 1
      emit({ type: 'candle', topic, candle: bar })
      forming.push(setTimeout(showForming, config.candles.everyMs / 2))
    }, config.candles.everyMs)
    timers.set(topic, () => {
      clearInterval(interval)
      for (const t of forming) clearTimeout(t)
    })
  }

  let nextOrder = 1
  const emitOrder = (order) => emit({ type: 'session', id: config.session.id, event: { event: 'order', order: structuredClone(order) } })

  window.__stubGoLive = () => {
    const session = config.session
    session.status = { status: 'live' }
    session.live_from = session.last_bar?.open_time ?? Date.now()
    emit({ type: 'session', id: session.id, event: { event: 'status', status: session.status } })
  }

  function placeOrder(body) {
    const session = config.session
    if (session.live_from === null) {
      // The engine's words (td-server error.rs, ManualError::NotLive).
      return fail(409, 'not_live', 'the session has not fed its first live bar yet; orders can be placed once it has')
    }
    const order = {
      source: 'manual',
      id: nextOrder++,
      request: {
        instrument: session.config.instrument.id,
        side: body.side,
        qty: body.qty,
        order_type: body.type === 'limit' ? { type: 'limit', price: body.limit_price } : { type: 'market' },
        tif: 'gtc',
        reduce_only: body.reduce_only ?? false,
        client_tag: null,
      },
      submitted_at: session.last_bar?.open_time ?? 0,
      decision_close: session.last_bar?.close ?? 0,
      attached_stop: null,
      status: { status: 'open' },
    }
    session.open_orders = [...session.open_orders, order]
    emitOrder(order)
    return { ok: true, status: 201, body: structuredClone(order) }
  }

  function cancelOrder(id) {
    const session = config.session
    const order = session.open_orders.find((o) => o.id === id)
    if (!order) return fail(404, 'not_found', `no order ${id} in this session`)
    const cancelled = { ...order, status: { status: 'cancelled', reason: 'cancelled manually' } }
    session.open_orders = session.open_orders.filter((o) => o.id !== id)
    emitOrder(cancelled)
    return ok(structuredClone(cancelled))
  }

  window.td = {
    mock: config.mock,
    async request(request) {
      calls.push(structuredClone(request))
      const { method, path } = request
      if (method === 'GET' && path === '/strategies' && config.hangStrategies) return new Promise(() => {})
      if (method === 'GET' && path === '/instruments') {
        if (config.hangInstruments) return new Promise(() => {})
        if (config.failInstruments) {
          return fail(502, 'upstream', `${request.query?.venue} did not answer: connection timed out`)
        }
      }
      if (method === 'GET' && path === '/sessions') {
        if (config.failSessions > 0) {
          config.failSessions -= 1
          return fail(500, 'internal', 'the session store did not answer')
        }
        return ok(config.session ? [summary(config.session)] : [])
      }
      if (method === 'POST' && path === '/sessions') {
        window.__stubStarts = (window.__stubStarts ?? 0) + 1
        const body = request.body
        const instrument = (config.instruments ?? data.instruments).find(
          (i) => i.id.venue === body.instrument.venue && i.id.symbol === body.instrument.symbol && i.id.kind === body.instrument.kind,
        )
        await new Promise((resolve) => setTimeout(resolve, 300))
        config.session = {
          id: `stub-${window.__stubStarts}`,
          config: { ...body, instrument, fees: null, max_leverage: 1 },
          status: { status: 'warming_up' },
          cash: body.starting_cash,
          equity: body.starting_cash,
          position: { qty: 0, avg_entry: 0, mark_price: 0, unrealised_pnl: 0, realised_pnl: 0, fees_paid: 0, funding_paid: 0 },
          equity_curve: [],
          open_orders: [],
          fills: [],
          trades: [],
          funding: [],
          live_from: null,
          last_bar: null,
          dropped: [],
          warnings: [],
        }
        return ok(summary(config.session))
      }
      if (method === 'GET' && config.session && path === `/sessions/${config.session.id}`) return ok(config.session)
      const order = /^\/sessions\/([\w-]+)\/orders(?:\/(\d+)\/cancel)?$/.exec(path)
      if (method === 'POST' && order && order[1] === config.session?.id) {
        return order[2] === undefined ? placeOrder(request.body) : cancelOrder(Number(order[2]))
      }
      if (method === 'GET' && path === '/candles') return candles(request.query)
      return route(request)
    },
    async subscribe(topic) {
      window.__stubSubscribed = [...(window.__stubSubscribed ?? []), topic]
      if (topic.startsWith('candles:')) emitCandles(topic)
      if (config.session && topic === `session:${config.session.id}`) {
        setTimeout(() => emit({ type: 'session_state', session: config.session }), 0)
      }
      if (topic !== 'jobs' || subscribed) return
      subscribed = true
      // Right after subscribing, the engine sends the state of every job.
      setTimeout(() => {
        for (const job of jobs.values()) emitJob(job)
      }, 0)
    },
    async unsubscribe(topic) {
      window.__stubSubscribed = (window.__stubSubscribed ?? []).filter((t) => t !== topic)
      if (topic === 'jobs') subscribed = false
      timers.get(topic)?.()
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
