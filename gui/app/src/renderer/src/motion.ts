// Motion helpers. Durations come from the theme's tokens so CSS and script use the same values.

import { useEffect, useState } from 'react'

/** Reads a duration token such as `--duration-count` in milliseconds. */
function durationToken(name: string, fallbackMs: number): number {
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  const match = /^([\d.]+)(ms|s)$/.exec(raw)
  if (!match) return fallbackMs
  return Number(match[1]) * (match[2] === 's' ? 1000 : 1)
}

function prefersReducedMotion(): boolean {
  return window.matchMedia('(prefers-reduced-motion: reduce)').matches
}

/** Ease-out cubic: quick to start, gentle to land, so the count settles on the true figure. */
function easeOut(t: number): number {
  const c = Math.min(1, Math.max(0, t))
  return 1 - (1 - c) ** 3
}

/**
 * How far a count-up has run, from 0 to 1. With `animate` false (a revisited result, or reduced
 * motion) it is 1 from the start, so numbers show settled.
 */
export function useCountUp(animate: boolean, onDone?: () => void): number {
  const [progress, setProgress] = useState(() => (animate && !prefersReducedMotion() ? 0 : 1))
  const done = progress === 1

  // One loop per mounted result: it starts while the count is unfinished and drives itself.
  useEffect(() => {
    if (done) return
    const duration = durationToken('--duration-count', 700)
    let frame = 0
    const start = performance.now()
    const tick = (now: number) => {
      const t = (now - start) / duration
      setProgress(t >= 1 ? 1 : easeOut(t))
      if (t < 1) frame = requestAnimationFrame(tick)
    }
    frame = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(frame)
  }, [done])

  useEffect(() => {
    if (done) onDone?.()
  }, [done, onDone])

  return progress
}
