// How a run's progress reads: the label names the step and its count, and the bar shows the whole
// test plan, so both describe the same run.

import type { RunProgress } from './source'

const clamp = (x: number) => Math.min(1, Math.max(0, x))

/** How far the whole test plan has run, from 0 to 1: the finished steps plus the current step's share. */
export function overallProgress(p: RunProgress): number {
  if (p.steps <= 0) return 0
  const within = p.total ? clamp((p.done ?? 0) / p.total) : clamp(p.fraction ?? 0)
  return clamp((p.step - 1 + within) / p.steps)
}

/** For example "Step 2 of 4 · Sweeping parameters, 48 of 48". */
export function progressLabel(p: RunProgress): string {
  const count = p.total ? `, ${p.done ?? 0} of ${p.total}` : ''
  return `Step ${p.step} of ${p.steps} · ${p.stage}${count}`
}
