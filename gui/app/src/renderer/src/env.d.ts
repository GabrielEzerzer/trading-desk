import type { TdBridge } from '@td/shared'

declare global {
  interface Window {
    td: TdBridge
  }
}
