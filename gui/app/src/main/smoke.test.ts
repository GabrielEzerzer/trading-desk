import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createSmoke } from './smoke'

function setup() {
  const calls: string[] = []
  const smoke = createSmoke({ pass: () => calls.push('pass'), fail: (reason) => calls.push(`fail ${reason}`) }, 1_000)
  return { smoke, calls }
}

describe('createSmoke', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('passes once both the engine and the window are up, in either order, and only once', () => {
    const { smoke, calls } = setup()
    smoke.engine({ state: 'connecting' })
    smoke.loaded()
    expect(calls).toEqual([])
    smoke.engine({ state: 'running' })
    smoke.loaded()
    vi.advanceTimersByTime(2_000)
    expect(calls).toEqual(['pass'])
  })

  it('fails when the engine fails, and ignores what comes after', () => {
    const { smoke, calls } = setup()
    smoke.loaded()
    smoke.engine({ state: 'failed', message: 'cannot start td-server' })
    smoke.engine({ state: 'running' })
    expect(calls).toEqual(['fail the engine failed: cannot start td-server'])
  })

  it('fails when the window does not load', () => {
    const { smoke, calls } = setup()
    smoke.loadFailed('ERR_FILE_NOT_FOUND')
    expect(calls).toEqual(['fail the window did not load: ERR_FILE_NOT_FOUND'])
  })

  it('fails at the time limit, naming what is missing', () => {
    const { smoke, calls } = setup()
    smoke.engine({ state: 'running' })
    vi.advanceTimersByTime(1_000)
    expect(calls).toEqual(['fail waited 1 s for the window load'])
  })
})
