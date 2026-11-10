import { useEffect, useState } from 'react'
import { EngineStatusIndicator } from './EngineStatus'
import { StrategyLab } from './sections/StrategyLab'
import { Trading } from './sections/Trading'

type Section = 'lab' | 'trading'

const SECTIONS: readonly { id: Section; label: string; digit: string }[] = [
  { id: 'lab', label: 'Strategy Lab', digit: '1' },
  { id: 'trading', label: 'Trading', digit: '2' },
]

export function App() {
  const [section, setSection] = useState<Section>('lab')

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.altKey || event.shiftKey || event.metaKey) return
      // `code`, not `key`, so the shortcut works on keyboard layouts where 1 is not the plain key.
      const target = SECTIONS.find((s) => event.code === `Digit${s.digit}` || event.code === `Numpad${s.digit}`)
      if (!target) return
      event.preventDefault()
      setSection(target.id)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  return (
    <div className="app">
      <header className="titlebar">
        <div className="toggle" role="group" aria-label="Section">
          {SECTIONS.map((s) => (
            <button
              key={s.id}
              type="button"
              aria-pressed={section === s.id}
              aria-keyshortcuts={`Control+${s.digit}`}
              title={`${s.label} (Ctrl+${s.digit})`}
              onClick={() => setSection(s.id)}
            >
              {s.label}
            </button>
          ))}
        </div>
        <EngineStatusIndicator />
      </header>
      <main className="content">{section === 'lab' ? <StrategyLab /> : <Trading />}</main>
    </div>
  )
}
