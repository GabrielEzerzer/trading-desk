import * as Tooltip from '@radix-ui/react-tooltip'
import type { EngineStatus } from '@td/shared'
import { sentence } from './engine'

/**
 * A small dot beside the section toggle, shown only while the engine starts or after it failed.
 * Hover or focus shows the plain message. A running engine needs no indicator.
 */
export function EngineDot({ status }: { status: EngineStatus }) {
  if (status.state === 'running') return null
  const message = status.state === 'failed' ? `The engine failed. ${sentence(status.message)}` : 'The engine is starting.'
  return (
    <Tooltip.Root>
      <Tooltip.Trigger asChild>
        <button type="button" className="engine-dot" data-state-engine={status.state} aria-label={message}>
          <span aria-hidden="true" />
        </button>
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content className="tooltip" side="bottom" align="start" sideOffset={8}>
          {message}
        </Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  )
}
