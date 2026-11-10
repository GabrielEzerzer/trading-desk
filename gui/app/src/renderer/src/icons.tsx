// The few icons the app draws, inline so they inherit the text colour. Decorative: the control
// that holds one carries the accessible name.

const common = {
  width: 20,
  height: 20,
  viewBox: '0 0 20 20',
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 1.6,
  strokeLinecap: 'round',
  strokeLinejoin: 'round',
  'aria-hidden': true,
  focusable: false,
} as const

/** Run: a triangle, the universal "start". */
export function PlayIcon() {
  return (
    <svg {...common} stroke="none" fill="currentColor">
      <path d="M7 4.8v10.4a.8.8 0 0 0 1.2.7l8.2-5.2a.8.8 0 0 0 0-1.4L8.2 4.1A.8.8 0 0 0 7 4.8Z" />
    </svg>
  )
}

/** Cancel: a square, as a media stop. */
export function StopIcon() {
  return (
    <svg {...common} stroke="none" fill="currentColor">
      <rect x="5.5" y="5.5" width="9" height="9" rx="1.5" />
    </svg>
  )
}

export function SlidersIcon() {
  return (
    <svg {...common}>
      <path d="M4 6h7M15 6h1M4 14h1M9 14h7" />
      <circle cx="13" cy="6" r="2" />
      <circle cx="7" cy="14" r="2" />
    </svg>
  )
}

export function ChevronIcon() {
  return (
    <svg {...common} width={16} height={16}>
      <path d="M6 8l4 4 4-4" />
    </svg>
  )
}

export function CheckIcon() {
  return (
    <svg {...common} width={16} height={16}>
      <path d="M5 10.5l3 3 7-7" />
    </svg>
  )
}

export function CrossIcon() {
  return (
    <svg {...common} width={16} height={16}>
      <path d="M6 6l8 8M14 6l-8 8" />
    </svg>
  )
}
