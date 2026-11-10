// In-page audits for the UI gates. Each function runs inside the page through `page.evaluate`, so
// it is self-contained and returns its offenders as data. They follow the ui-gates skill.

export interface LayoutReport {
  overflow: unknown[]
  clipped: unknown[]
  overlap: unknown[]
  brokenImg: unknown[]
}

/**
 * Horizontal overflow, clipped text, overlapping text and controls, and broken images.
 *
 * Boxes are measured where they can be seen: clipped to every ancestor that clips its overflow, so
 * content scrolled out of the Lab's column is not counted as overlapping the title bar.
 */
export function layoutAudit(): LayoutReport {
  const out: LayoutReport = { overflow: [], clipped: [], overlap: [], brokenImg: [] }
  const vw = document.documentElement.clientWidth
  if (document.documentElement.scrollWidth > vw) out.overflow.push({ scrollWidth: document.documentElement.scrollWidth, vw })
  const sel = (e: Element) =>
    e.tagName.toLowerCase() +
    (e.id ? `#${e.id}` : '') +
    (typeof e.className === 'string' && e.className ? `.${e.className.trim().split(/\s+/)[0]}` : '')
  const vis = (e: Element) => {
    const s = getComputedStyle(e)
    const r = e.getBoundingClientRect()
    return s.visibility !== 'hidden' && s.display !== 'none' && +s.opacity > 0 && r.width > 1 && r.height > 1
  }
  const seen = (e: Element) => {
    const r = e.getBoundingClientRect()
    let { left, top, right, bottom } = r
    for (let a = e.parentElement; a; a = a.parentElement) {
      const s = getComputedStyle(a)
      if (s.overflowX === 'visible' && s.overflowY === 'visible') continue
      const c = a.getBoundingClientRect()
      left = Math.max(left, c.left)
      top = Math.max(top, c.top)
      right = Math.min(right, c.right)
      bottom = Math.min(bottom, c.bottom)
    }
    return right - left > 1 && bottom - top > 1 ? { left, top, right, bottom } : null
  }

  for (const e of document.body.querySelectorAll('*')) {
    if (!vis(e)) continue
    const r = seen(e)
    if (r && (r.right > vw + 1 || r.left < -1)) out.overflow.push({ el: sel(e), left: r.left, right: r.right })
    const s = getComputedStyle(e)
    // A box that clips or scrolls sideways must not have wider content than it shows.
    if (s.overflowX !== 'visible' && !s.textOverflow.includes('ellipsis') && e.scrollWidth > e.clientWidth + 1)
      out.overflow.push({ el: sel(e), scrollWidth: e.scrollWidth, clientWidth: e.clientWidth })
    const own = [...e.childNodes].some((n) => n.nodeType === 3 && n.textContent?.trim())
    if (
      own &&
      s.overflow !== 'visible' &&
      (e.scrollWidth > e.clientWidth + 1 || e.scrollHeight > e.clientHeight + 1) &&
      !s.textOverflow.includes('ellipsis') &&
      !/auto|scroll/.test(s.overflow)
    )
      out.clipped.push({ el: sel(e), text: e.textContent?.trim().slice(0, 40) })
    if (e instanceof HTMLImageElement && e.complete && e.naturalWidth === 0) out.brokenImg.push({ src: e.src })
  }

  // Intentional layers, allowed to cover the page: the composer floats over the scrolling column
  // (the fade hides what passes under it; the clearance spec checks the last content clears it),
  // a table's sticky header covers the rows scrolled under it, and menus and tooltips float above
  // everything.
  const layer = (e: Element) =>
    e.closest('[role=menu], [role=tooltip], .tooltip')
      ? 'popover'
      : e.closest('.composer-dock')
        ? 'composer'
        : e.closest('.table thead')
          ? 'sticky-header'
          : 'page'
  const boxes = [
    ...document.querySelectorAll('a,button,input,select,textarea,h1,h2,h3,h4,p,li,label,img,dt,dd,th,td,[role^=menuitem],[role=radio]'),
  ]
    .filter(vis)
    .map((e) => ({ e, r: seen(e), layer: layer(e) }))
    .filter((b): b is { e: Element; r: NonNullable<ReturnType<typeof seen>>; layer: string } => b.r !== null)
  for (let i = 0; i < boxes.length; i++)
    for (let j = i + 1; j < boxes.length; j++) {
      const a = boxes[i]
      const b = boxes[j]
      if (!a || !b || a.e.contains(b.e) || b.e.contains(a.e)) continue
      if (a.layer !== b.layer) continue
      const w = Math.min(a.r.right, b.r.right) - Math.max(a.r.left, b.r.left)
      const h = Math.min(a.r.bottom, b.r.bottom) - Math.max(a.r.top, b.r.top)
      if (w > 2 && h > 2) out.overlap.push({ a: sel(a.e), b: sel(b.e), w: Math.round(w), h: Math.round(h) })
    }
  return out
}

/**
 * Hit areas: every visible control must answer clicks across a square of `min` CSS px around its
 * centre (the composer's controls 44, the rest 24, WCAG 2.5.8). Tests with elementFromPoint at the
 * centre plus and minus (min / 2 - 1) px on each axis, after scrolling the control to the middle.
 */
export function hitAudit(): unknown[] {
  const failures: unknown[] = []
  const sel = (e: Element) =>
    e.tagName.toLowerCase() +
    (e.getAttribute('aria-label') ? `[aria-label="${e.getAttribute('aria-label')}"]` : '') +
    (typeof e.className === 'string' && e.className ? `.${e.className.trim().split(/\s+/)[0]}` : '') +
    (e.textContent?.trim() ? ` "${e.textContent.trim().slice(0, 30)}"` : '')
  const controls = [
    ...document.querySelectorAll('a,button,input,select,textarea,summary,[role=button],[role=radio],[role^=menuitem]'),
  ].filter((e) => {
    const s = getComputedStyle(e)
    const r = e.getBoundingClientRect()
    return s.visibility !== 'hidden' && s.display !== 'none' && r.width > 1 && r.height > 1
  })
  const answers = (e: Element, hit: Element | null) => {
    if (!hit) return false
    if (hit === e || e.contains(hit)) return true
    const label = hit.closest('label')
    return !!label && (label.contains(e) || (label.htmlFor !== '' && label.htmlFor === e.id))
  }
  for (const e of controls) {
    const min = e.closest('.composer') ? 44 : 24
    e.scrollIntoView({ block: 'center', inline: 'center' })
    const r = e.getBoundingClientRect()
    const cx = r.left + r.width / 2
    const cy = r.top + r.height / 2
    const d = min / 2 - 1
    const missed = [
      [-d, -d],
      [d, -d],
      [-d, d],
      [d, d],
    ].filter(([dx = 0, dy = 0]) => !answers(e, document.elementFromPoint(cx + dx, cy + dy)))
    if (missed.length) failures.push({ el: sel(e), min, size: [Math.round(r.width), Math.round(r.height)], missed })
  }
  return failures
}

const LAYOUT_PROPERTY =
  /^(width|height|top|left|right|bottom|inset(-.*)?|margin(-.*)?|padding(-.*)?|border(-(top|right|bottom|left))?-width|min-(width|height)|max-(width|height))$/

export interface MotionReport {
  layoutTransitions: unknown[]
  transitionAll: unknown[]
  layoutKeyframes: unknown[]
}

/**
 * Transitions and keyframe animations that animate layout properties, and `transition: all`, over
 * every element and its ::before and ::after. Pass it LAYOUT_PROPERTY_SOURCE:
 * `page.evaluate(motionAudit, LAYOUT_PROPERTY_SOURCE)`.
 */
export function motionAudit(layoutSource: string): MotionReport {
  const layout = new RegExp(layoutSource)
  const out: MotionReport = { layoutTransitions: [], transitionAll: [], layoutKeyframes: [] }
  const sel = (e: Element) =>
    e.tagName.toLowerCase() + (typeof e.className === 'string' && e.className ? `.${e.className.trim().split(/\s+/)[0]}` : '')
  const secs = (value: string) => (value.endsWith('ms') ? parseFloat(value) / 1000 : parseFloat(value))
  for (const e of document.querySelectorAll('*')) {
    for (const pseudo of [null, '::before', '::after']) {
      const s = getComputedStyle(e, pseudo)
      const props = s.transitionProperty.split(',').map((p) => p.trim())
      const durations = s.transitionDuration.split(',').map((d) => secs(d.trim()))
      props.forEach((p, i) => {
        const duration = durations[i % durations.length] ?? 0
        if (duration <= 0) return
        const where = { el: sel(e) + (pseudo ?? ''), property: p, duration: s.transitionDuration }
        if (p === 'all') out.transitionAll.push(where)
        else if (layout.test(p)) out.layoutTransitions.push(where)
      })
    }
  }
  for (const a of document.getAnimations()) {
    const effect = a.effect as KeyframeEffect | null
    const props = new Set(
      (effect?.getKeyframes() ?? []).flatMap((k) =>
        Object.keys(k).filter((p) => !['offset', 'computedOffset', 'easing', 'composite'].includes(p)),
      ),
    )
    const kebab = [...props].map((p) => p.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`))
    const bad = kebab.filter((p) => layout.test(p))
    if (bad.length)
      out.layoutKeyframes.push({
        el: effect?.target ? sel(effect.target) : null,
        name: (a as CSSAnimation).animationName ?? null,
        properties: bad,
        timing: effect?.getTiming(),
      })
  }
  return out
}

/** The source of the layout-property pattern, passed into the page where closures do not reach. */
export const LAYOUT_PROPERTY_SOURCE = LAYOUT_PROPERTY.source

/**
 * Motion that should stop under reduced motion: transitions and animations that move or scale
 * (transform, translate, scale, rotate or position) and last longer than 1 ms, and any infinite
 * animation. Loading indicators (role=progressbar, aria-busy=true) are exempt.
 */
export function reducedMotionAudit(): unknown[] {
  const moving = /^(transform|translate|scale|rotate|top|left|right|bottom|inset.*|offset.*)$/
  const out: unknown[] = []
  const sel = (e: Element) =>
    e.tagName.toLowerCase() + (typeof e.className === 'string' && e.className ? `.${e.className.trim().split(/\s+/)[0]}` : '')
  const loading = (e: Element) => !!e.closest('[role=progressbar], [aria-busy=true]')
  const secs = (value: string) => (value.endsWith('ms') ? parseFloat(value) / 1000 : parseFloat(value))
  for (const e of document.querySelectorAll('*')) {
    if (loading(e)) continue
    for (const pseudo of [null, '::before', '::after']) {
      const s = getComputedStyle(e, pseudo)
      const props = s.transitionProperty.split(',').map((p) => p.trim())
      const durations = s.transitionDuration.split(',').map((d) => secs(d.trim()))
      props.forEach((p, i) => {
        const duration = durations[i % durations.length] ?? 0
        if (duration > 0.001 && (p === 'all' || moving.test(p)))
          out.push({ el: sel(e) + (pseudo ?? ''), transition: p, duration: s.transitionDuration })
      })
    }
  }
  for (const a of document.getAnimations()) {
    const effect = a.effect as KeyframeEffect | null
    const target = effect?.target
    if (target && loading(target)) continue
    const timing = effect?.getComputedTiming()
    const props = (effect?.getKeyframes() ?? []).flatMap((k) => Object.keys(k))
    const moves = props.some((p) => moving.test(p.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)))
    const long = Number(timing?.duration ?? 0) > 1
    if ((moves && long) || timing?.iterations === Infinity)
      out.push({ el: target ? sel(target) : null, name: (a as CSSAnimation).animationName ?? null, timing: effect?.getTiming() })
  }
  return out
}

/**
 * Infinite animations on anything that is not a loading indicator. `.shimmer` is allowlisted: it is
 * the running cue on the status text of work in progress (engine starting, a run), so it is a
 * loading indicator, and it stops under reduced motion.
 */
export function infiniteAudit(): unknown[] {
  const out: unknown[] = []
  for (const a of document.getAnimations()) {
    const effect = a.effect as KeyframeEffect | null
    const target = effect?.target
    if (effect?.getTiming().iterations !== Infinity) continue
    if (target?.closest('[role=progressbar], [aria-busy=true], .shimmer')) continue
    out.push({ el: target?.tagName ?? null, name: (a as CSSAnimation).animationName ?? null, timing: effect.getTiming() })
  }
  return out
}
