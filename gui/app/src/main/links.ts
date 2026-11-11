// Links the renderer may open in the system browser. Every other window the page asks for is
// denied (index.ts), so the page never navigates away from the app.

/**
 * The chart library's licence (lightweight-charts, Apache-2.0) asks for a link to TradingView
 * where users can see it; the chart shows it as "Charting by TradingView".
 */
export const TRADINGVIEW_URL = 'https://www.tradingview.com/'

const EXTERNAL = new Set([TRADINGVIEW_URL])

/** True only for an exact URL on the list: no prefixes, queries or lookalikes. */
export function isExternalLink(url: string): boolean {
  return EXTERNAL.has(url)
}
