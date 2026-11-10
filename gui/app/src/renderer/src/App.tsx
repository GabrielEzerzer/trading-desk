import * as Tooltip from '@radix-ui/react-tooltip'
import { useEffect, useState } from 'react'
import { EngineDot } from './EngineDot'
import { EngineState } from './EngineState'
import { useEngineStatus } from './engine'
import { StrategyLab } from './lab/StrategyLab'
import { SECTIONS, type Section, SectionToggle } from './SectionToggle'
import { Trading } from './sections/Trading'

export function App() {
  const [section, setSection] = useState<Section>('lab')
  const status = useEngineStatus()

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

  const label = SECTIONS.find((s) => s.id === section)?.label

  return (
    <Tooltip.Provider delayDuration={300}>
      <div className="app">
        <header className="titlebar">
          <SectionToggle value={section} onChange={setSection} />
          <EngineDot status={status} />
        </header>
        <main className="sheet" aria-label={label}>
          {/* Both sections stay mounted so a run keeps going while Trading is open. A hidden
              section's fade restarts when it is shown again: the switch is one 180 ms fade. */}
          <div className="sheet-content" hidden={section !== 'lab'}>
            {status.state === 'running' ? <StrategyLab /> : <EngineState status={status} />}
          </div>
          <div className="sheet-content" hidden={section !== 'trading'}>
            <Trading />
          </div>
        </main>
      </div>
    </Tooltip.Provider>
  )
}
