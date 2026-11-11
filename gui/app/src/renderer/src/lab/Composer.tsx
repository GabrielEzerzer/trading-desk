import * as Menu from '@radix-ui/react-dropdown-menu'
import * as Tooltip from '@radix-ui/react-tooltip'
import type { InstrumentId, Timeframe } from '@td/shared'
import { type Ref, useImperativeHandle, useRef } from 'react'
import { CheckIcon, ChevronIcon, PlayIcon, SlidersIcon, StopIcon } from '../icons'
import { formatInstrument } from './format'
import { TIMEFRAMES, instrumentKey } from './runs'
import type { LabStrategy } from './source'

export interface ComposerHandle {
  /** Focuses the strategy pill, which shows the choice just made. */
  focusPicker(): void
}

interface Props {
  strategies: LabStrategy[]
  selected: LabStrategy | null
  running: boolean
  /** Why Run cannot start now, or null when it can. */
  runBlocked: string | null
  /** The instruments with candles stored; null while they load. */
  datasets: InstrumentId[] | null
  instrument: InstrumentId | null
  timeframe: Timeframe
  onInstrument(key: string): void
  onTimeframe(timeframe: Timeframe): void
  onSelect(name: string | null): void
  onRun(): void
  onCancel(): void
  ref?: Ref<ComposerHandle>
}

/**
 * Floats at the bottom of the reading column: the strategy pill, the overrides icon (the data and
 * timeframe a run uses) and the one dark shape in the window, Run. While a run is in progress Run becomes Cancel. The slot is where
 * typed chat arrives later (#9).
 */
export function Composer({
  strategies,
  selected,
  running,
  runBlocked,
  datasets,
  instrument,
  timeframe,
  onInstrument,
  onTimeframe,
  onSelect,
  onRun,
  onCancel,
  ref,
}: Props) {
  const pickerRef = useRef<HTMLButtonElement>(null)
  useImperativeHandle(ref, () => ({ focusPicker: () => pickerRef.current?.focus({ preventScroll: true }) }), [])

  const runLabel = running ? 'Cancel the run' : 'Run'
  const runButton = (
    <button
      type="button"
      className="run"
      aria-label={runLabel}
      aria-disabled={!running && runBlocked !== null}
      onClick={() => {
        if (running) onCancel()
        else if (runBlocked === null) onRun()
      }}
    >
      {running ? <StopIcon /> : <PlayIcon />}
    </button>
  )

  return (
    <div className="composer-dock">
      <div className="composer" role="group" aria-label="Run a strategy">
        <Menu.Root modal={false}>
          <Menu.Trigger asChild disabled={running}>
            <button ref={pickerRef} type="button" className="pill" title={running ? 'A run is in progress' : undefined}>
              <span className="pill-label">{selected?.label ?? 'Choose a strategy'}</span>
              <ChevronIcon />
            </button>
          </Menu.Trigger>
          <Menu.Portal>
            <Menu.Content className="menu" side="top" align="start" sideOffset={10} collisionPadding={16}>
              <Menu.RadioGroup value={selected?.name ?? ''} onValueChange={(name) => onSelect(name)}>
                {strategies.map((s) => (
                  <Menu.RadioItem key={s.name} value={s.name} className="menu-item">
                    <Menu.ItemIndicator className="menu-check">
                      <CheckIcon />
                    </Menu.ItemIndicator>
                    <span className="menu-label">{s.label}</span>
                    {s.kind && <span className="menu-hint">{s.kind}</span>}
                  </Menu.RadioItem>
                ))}
              </Menu.RadioGroup>
              {selected && (
                <>
                  <Menu.Separator className="menu-separator" />
                  <Menu.Item className="menu-item" onSelect={() => onSelect(null)}>
                    <span className="menu-label">All strategies</span>
                  </Menu.Item>
                </>
              )}
            </Menu.Content>
          </Menu.Portal>
        </Menu.Root>

        <Menu.Root modal={false}>
          <Tooltip.Root>
            <Tooltip.Trigger asChild>
              <Menu.Trigger asChild>
                <button type="button" className="icon-button" aria-label="Overrides">
                  <SlidersIcon />
                </button>
              </Menu.Trigger>
            </Tooltip.Trigger>
            <Tooltip.Portal>
              <Tooltip.Content className="tooltip" side="top" sideOffset={8}>
                Overrides
              </Tooltip.Content>
            </Tooltip.Portal>
          </Tooltip.Root>
          <Menu.Portal>
            <Menu.Content className="menu" side="top" align="start" sideOffset={10} collisionPadding={16}>
              <Menu.Label className="menu-heading">Data</Menu.Label>
              {datasets && datasets.length > 0 ? (
                <Menu.RadioGroup value={instrument ? instrumentKey(instrument) : ''} onValueChange={onInstrument}>
                  {datasets.map((id) => (
                    <Menu.RadioItem key={instrumentKey(id)} value={instrumentKey(id)} className="menu-item" disabled={running}>
                      <Menu.ItemIndicator className="menu-check">
                        <CheckIcon />
                      </Menu.ItemIndicator>
                      <span className="menu-label">{formatInstrument(id)}</span>
                    </Menu.RadioItem>
                  ))}
                </Menu.RadioGroup>
              ) : (
                <Menu.Item className="menu-item" disabled>
                  <span className="menu-label">{datasets ? 'No data downloaded' : 'Loading data'}</span>
                </Menu.Item>
              )}
              <Menu.Separator className="menu-separator" />
              <Menu.Label className="menu-heading">Timeframe</Menu.Label>
              <Menu.RadioGroup value={timeframe} onValueChange={(value) => onTimeframe(value as Timeframe)}>
                {TIMEFRAMES.map((tf) => (
                  <Menu.RadioItem key={tf} value={tf} className="menu-item" disabled={running}>
                    <Menu.ItemIndicator className="menu-check">
                      <CheckIcon />
                    </Menu.ItemIndicator>
                    <span className="menu-label">{tf}</span>
                  </Menu.RadioItem>
                ))}
              </Menu.RadioGroup>
              <Menu.Separator className="menu-separator" />
              <Menu.Item className="menu-item" disabled>
                <span className="menu-stack">
                  <span className="menu-label">Override parameters</span>
                  <span className="menu-hint">Not available yet</span>
                </span>
              </Menu.Item>
            </Menu.Content>
          </Menu.Portal>
        </Menu.Root>

        <span className="composer-spacer" />

        {/* Icon-only, so it always has a tooltip: its name, or why it cannot run. */}
        <Tooltip.Root>
          <Tooltip.Trigger asChild>{runButton}</Tooltip.Trigger>
          <Tooltip.Portal>
            <Tooltip.Content className="tooltip" side="top" align="end" sideOffset={8}>
              {running ? runLabel : (runBlocked ?? runLabel)}
            </Tooltip.Content>
          </Tooltip.Portal>
        </Tooltip.Root>
      </div>
    </div>
  )
}
