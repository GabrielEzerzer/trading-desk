import { useRef } from 'react'

export type Section = 'lab' | 'trading'

export const SECTIONS: readonly { id: Section; label: string; digit: string }[] = [
  { id: 'lab', label: 'Strategy Lab', digit: '1' },
  { id: 'trading', label: 'Trading', digit: '2' },
]

/**
 * The two sections as a radio group in one white pill. The thumb slides on the spring token; the
 * arrow keys move between the options as in any radio group, and Ctrl+1 / Ctrl+2 work anywhere
 * (App.tsx).
 */
export function SectionToggle({ value, onChange }: { value: Section; onChange: (section: Section) => void }) {
  const refs = useRef<(HTMLButtonElement | null)[]>([])
  const index = SECTIONS.findIndex((s) => s.id === value)

  const onKeyDown = (event: React.KeyboardEvent) => {
    const step = event.key === 'ArrowRight' || event.key === 'ArrowDown' ? 1 : event.key === 'ArrowLeft' || event.key === 'ArrowUp' ? -1 : 0
    if (step === 0) return
    event.preventDefault()
    const next = (index + step + SECTIONS.length) % SECTIONS.length
    const section = SECTIONS[next]
    if (!section) return
    onChange(section.id)
    refs.current[next]?.focus()
  }

  return (
    <div className="toggle" role="radiogroup" aria-label="Section" data-index={index} onKeyDown={onKeyDown}>
      <span className="toggle-thumb" aria-hidden="true" />
      {SECTIONS.map((s, i) => (
        <button
          key={s.id}
          ref={(el) => {
            refs.current[i] = el
          }}
          type="button"
          role="radio"
          aria-checked={value === s.id}
          tabIndex={value === s.id ? 0 : -1}
          aria-keyshortcuts={`Control+${s.digit}`}
          title={`${s.label} (Ctrl+${s.digit})`}
          onClick={() => onChange(s.id)}
        >
          {s.label}
        </button>
      ))}
    </div>
  )
}
