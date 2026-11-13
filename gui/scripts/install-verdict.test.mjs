import { describe, expect, it } from 'vitest'
import { WATCHDOG_NOTE, launchVerdict } from './install-verdict.mjs'

const engineBin = 'C:\\T\\app\\resources\\td-server.exe'
const dataDir = 'C:\\T\\data'
const good = [
  `[td-server] td-server: listening on 127.0.0.1:5000, data in ${dataDir}`,
  `TD_SMOKE ok engine=${engineBin}`,
  '[td-server] stopped (clean)',
]
const verdict = (code, output = good) => () => launchVerdict({ code, output, engineBin, dataDir })

describe('launchVerdict', () => {
  it('passes plainly on exit code 0, and with the watchdog note on 1', () => {
    expect(verdict(0)()).toBe('')
    expect(verdict(1)()).toBe(WATCHDOG_NOTE)
  })

  it('fails on any other exit code, or a timeout', () => {
    expect(verdict(3)).toThrow('the app exited with code 3')
    expect(verdict('timeout')).toThrow('still running')
  })

  it('fails when a piece is missing, whatever the exit code', () => {
    expect(verdict(1, good.slice(0, 2))).toThrow('did not stop cleanly')
    expect(verdict(0, [good[0], good[2]])).toThrow('without reporting the smoke result')
    expect(verdict(0, good.slice(1))).toThrow('fresh data folder')
    expect(verdict(0, [good[0], 'TD_SMOKE ok engine=C:\\elsewhere\\td-server.exe', good[2]])).toThrow('not C:')
  })

  it("reports the smoke's own failure", () => {
    expect(verdict(2, ['TD_SMOKE fail: the engine failed: boom'])).toThrow('TD_SMOKE fail: the engine failed: boom (exit code 2)')
  })
})
