import type { ApiRequest, ApiResult, DownloadRequest, InstrumentId, JobInfo, ServerMessage } from '@td/shared'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { HISTORY_FROM, NO_OANDA_TOKEN, engineLabSource } from './engine-source'

// A stand-in for the preload's `window.td`: POST /data/download answers with job 1, and `end`
// publishes how that job ended on the `jobs` topic.
function fakeBridge() {
  const listeners = new Set<(message: ServerMessage) => void>()
  const downloads: DownloadRequest[] = []
  const td = {
    subscribe: vi.fn(async (_topic: string) => {}),
    onMessage: vi.fn((listener: (message: ServerMessage) => void) => {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    }),
    request: vi.fn(async (request: ApiRequest): Promise<ApiResult> => {
      if (request.method === 'POST' && request.path === '/data/download') {
        downloads.push(request.body as DownloadRequest)
        return { ok: true, status: 202, body: { job_id: 1 } }
      }
      return { ok: false, status: 404, error: { code: 'not_found', message: request.path } }
    }),
  }
  const end = (job: Partial<JobInfo>) => {
    for (const listener of [...listeners]) listener({ type: 'job', job: { id: 1, ...job } as JobInfo })
  }
  return { td, downloads, end }
}

const btc: InstrumentId = { venue: 'binance', symbol: 'BTCUSDT', kind: 'spot' }
const usdjpy: InstrumentId = { venue: 'oanda', symbol: 'USDJPY', kind: 'fx' }

let bridge: ReturnType<typeof fakeBridge>

beforeEach(() => {
  bridge = fakeBridge()
  vi.stubGlobal('window', { td: bridge.td })
})

afterEach(() => vi.unstubAllGlobals())

/** Starts a download and waits until its request has reached the engine. */
async function download(instrument: InstrumentId) {
  const job = engineLabSource.download(instrument, () => {})
  await vi.waitFor(() => expect(bridge.downloads).toHaveLength(1))
  return job
}

describe('engine download', () => {
  it("starts each venue's history at the earliest date the engine accepts", async () => {
    expect(HISTORY_FROM).toEqual({
      binance: Date.UTC(2017, 0, 1),
      bybit: Date.UTC(2017, 0, 1),
      oanda: Date.UTC(2007, 0, 1),
    })
    const fx = await download(usdjpy)
    expect(bridge.downloads[0]).toMatchObject({ instrument: usdjpy, from: Date.UTC(2007, 0, 1) })
    bridge.end({ status: 'done' })
    await fx.result

    bridge.downloads.length = 0
    const crypto = await download(btc)
    expect(bridge.downloads[0]).toMatchObject({ instrument: btc, from: Date.UTC(2017, 0, 1) })
    bridge.end({ status: 'done' })
    await crypto.result
  })

  it('says the OANDA token is missing when the engine fails the download for it', async () => {
    const job = await download(usdjpy)
    // The engine's failed job (td-data's MissingOandaToken, td-server's bad_request).
    bridge.end({
      status: 'failed',
      error: {
        code: 'bad_request',
        message: "oanda's history needs an API token: set OANDA_API_TOKEN to the token of an OANDA practice account",
      },
    })
    await expect(job.result).rejects.toThrow(NO_OANDA_TOKEN)
  })

  it("passes on any other failure in the engine's own words", async () => {
    const job = await download(usdjpy)
    const message =
      'https://api-fxpractice.oanda.com/v3/instruments/USD_JPY/candles was refused (401: Insufficient ' +
      'authorization to perform request.): the token in OANDA_API_TOKEN is not valid, or is not a ' +
      "practice account's token"
    bridge.end({ status: 'failed', error: { code: 'bad_request', message } })
    await expect(job.result).rejects.toThrow(message)
  })
})
