import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterAll, beforeAll, describe, expect, it } from 'vitest'
import { STUB_PATH, createStubServer } from './serve.mjs'

const INDEX =
  '<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="script-src \'self\'">' +
  '<script type="module" crossorigin src="./assets/index.js"></script></head><body><div id="root"></div></body></html>'

let root
let server
let origin

beforeAll(async () => {
  root = mkdtempSync(join(tmpdir(), 'td-serve-'))
  mkdirSync(join(root, 'assets'))
  writeFileSync(join(root, 'index.html'), INDEX)
  writeFileSync(join(root, 'assets', 'index.js'), 'export {}')
  server = createStubServer({ root })
  await new Promise((done) => server.listen(0, '127.0.0.1', done))
  origin = `http://127.0.0.1:${server.address().port}`
})

afterAll(() => {
  server.close()
  rmSync(root, { recursive: true, force: true })
})

const stubConfigAt = async (src) => {
  const body = await (await fetch(new URL(src, origin))).text()
  const call = 'installBridgeStub('
  return JSON.parse(body.slice(body.lastIndexOf(call) + call.length, body.trimEnd().length - 1))
}

describe('pnpm serve', () => {
  it("serves the index with the stub injected before the app's scripts, carrying the page's query", async () => {
    const res = await fetch(`${origin}/?preset=engine&noData=true`)
    const html = await res.text()
    expect(res.status).toBe(200)
    const stub = html.indexOf(`<script src="${STUB_PATH}?preset=engine&amp;noData=true"></script>`)
    expect(stub).toBeGreaterThan(html.indexOf('Content-Security-Policy'))
    expect(stub).toBeLessThan(html.indexOf('./assets/index.js'))
    expect((await fetch(`${origin}/assets/index.js`)).status).toBe(200)
  })

  it('selects the stub configuration from the URL: a preset, then single fields', async () => {
    expect(await stubConfigAt('/__bridge-stub.js')).toMatchObject({ engine: 'running', mock: true, hangStrategies: false })
    const engine = await stubConfigAt('/__bridge-stub.js?preset=engine&noData=true&download=no_token')
    expect(engine).toMatchObject({ engine: 'running', mock: false, noData: true, download: 'no_token' })
    const trading = await stubConfigAt(`/__bridge-stub.js?preset=engine-trading&history=${encodeURIComponent('{"*":"hang"}')}`)
    expect(trading.history).toEqual({ '*': 'hang' })
    expect(trading.candles.everyMs).toBe(250)
    expect(trading.engineData.strategies.length).toBeGreaterThan(0)
  })

  it('refuses an unknown preset and paths outside the build', async () => {
    const bad = await fetch(`${origin}/?preset=nope`)
    expect(bad.status).toBe(400)
    expect((await fetch(`${origin}/?preset=constructor`)).status).toBe(400)
    expect(await bad.text()).toContain('engine-trading')
    expect((await fetch(`${origin}/..%2fpackage.json`)).status).toBe(403)
    expect((await fetch(`${origin}/%E0%A4%A`)).status).toBe(400)
    expect((await fetch(`${origin}/assets/index.js`)).status).toBe(200)
  })
})
