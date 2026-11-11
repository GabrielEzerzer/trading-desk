import { describe, expect, it } from 'vitest'
import { overallProgress, progressLabel } from './progress'

describe('run progress', () => {
  it('counts finished steps plus the share of the current one', () => {
    expect(overallProgress({ step: 1, steps: 4, stage: 'Loading candles' })).toBe(0)
    expect(overallProgress({ step: 2, steps: 4, stage: 'Sweeping parameters', done: 24, total: 48 })).toBe(0.375)
    expect(overallProgress({ step: 2, steps: 4, stage: 'Sweeping parameters', done: 48, total: 48 })).toBe(0.5)
    expect(overallProgress({ step: 4, steps: 4, stage: 'Testing out of sample' })).toBe(0.75)
  })

  it('uses the share a step reports when it does not count, as a backtest job does', () => {
    const job = { step: 1, steps: 1, stage: 'Running the backtest', fraction: 0.4 }
    expect(overallProgress(job)).toBe(0.4)
    expect(overallProgress({ ...job, fraction: 3 })).toBe(1)
    expect(progressLabel(job)).toBe('Step 1 of 1 · Running the backtest')
  })

  it('stays between 0 and 1 on bad counts', () => {
    expect(overallProgress({ step: 5, steps: 4, stage: 'x', done: 9, total: 4 })).toBe(1)
    expect(overallProgress({ step: 1, steps: 0, stage: 'x' })).toBe(0)
  })

  it('names the step and its count in the label', () => {
    expect(progressLabel({ step: 2, steps: 4, stage: 'Sweeping parameters', done: 48, total: 48 })).toBe(
      'Step 2 of 4 · Sweeping parameters, 48 of 48',
    )
    expect(progressLabel({ step: 4, steps: 4, stage: 'Testing out of sample' })).toBe('Step 4 of 4 · Testing out of sample')
  })
})
