// Test-only stand-in for the preload's `window.td`, injected by shots.mjs and the UI gates (gui/e2e)
// before the page loads.
// It implements the TdBridge contract (gui/shared/src/bridge.ts) with a scripted engine status, and
// answers the engine's routes from the fixtures in app/mock/engine.json: the strategy list, saved
// runs, stored-data coverage, instrument rules, and backtest and download jobs whose changes arrive
// on the `jobs` topic like the engine's. Never part of the app bundle.
//
// `config`: { engine: 'starting' | 'failed' | 'running', mock: boolean, hangStrategies: boolean,
//             engineData: engine.json, noData?: boolean,
//             backtest?: 'ok' | 'data_missing' | 'too_many_runs' | 'fails', stepMs?: number }
//
// Every request is recorded in `window.__tdCalls`, so a flow can check what the app sent.

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

  const ok = (body) => ({ ok: true, status: 200, body })
  const fail = (status, code, message) => ({ ok: false, status, error: { code, message } })

  const emit = (job) => {
    if (!subscribed) return
    for (const listener of messageListeners) listener({ type: 'job', job: structuredClone(job) })
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
    emit(job)
    let i = 0
    const tick = () => {
      // A queued job is cancelled at once, by the cancel route.
      if (job.status === 'cancelled') return
      if (job.cancel_requested && i < steps.length) {
        Object.assign(job, { status: 'cancelled', message: 'cancelled', finished_at: Date.now() })
        emit(job)
        return
      }
      const step = steps[i++]
      if (step) {
        Object.assign(job, { status: 'running', started_at: job.started_at ?? Date.now() }, step)
        emit(job)
        setTimeout(tick, stepMs)
        return
      }
      Object.assign(job, { finished_at: Date.now() }, finish())
      emit(job)
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
      const instruments = data.instruments.filter((i) => i.id.venue === query.venue && i.id.kind === query.kind)
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
      emit(job)
      return { ok: true, status: 202, body: structuredClone(job) }
    }
    if (method === 'GET' && m && !m[2]) return ok(structuredClone(job))
    if (method === 'GET' && m?.[2] === 'result') {
      if (job.status !== 'done') return fail(409, 'job_not_done', `job ${job.id} is ${job.status}, not done yet`)
      return ok(data.details[job.result.run_id])
    }
    return fail(404, 'not_found', `${method} ${path} is not stubbed`)
  }

  window.td = {
    mock: config.mock,
    async request(request) {
      calls.push(structuredClone(request))
      if (request.method === 'GET' && request.path === '/strategies' && config.hangStrategies) return new Promise(() => {})
      return route(request)
    },
    async subscribe(topic) {
      if (topic !== 'jobs' || subscribed) return
      subscribed = true
      // Right after subscribing, the engine sends the state of every job.
      setTimeout(() => {
        for (const job of jobs.values()) emit(job)
      }, 0)
    },
    async unsubscribe(topic) {
      if (topic === 'jobs') subscribed = false
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
