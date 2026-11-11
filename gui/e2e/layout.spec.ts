// Per window size and per state, the Strategy Lab's and Trading's: layout invariants, hit areas and
// axe (WCAG 2.2 AA, contrast included).
import AxeBuilder from '@axe-core/playwright'
import { SIZES, STATES, realTimers, settle } from '../app/shots/harness.mjs'
import { hitAudit, layoutAudit } from './audit'
import { expect, test } from './fixtures'

for (const size of SIZES) {
  test.describe(`${size.name}`, () => {
    test.use({ viewport: { width: size.width, height: size.height } })

    for (const state of STATES) {
      test(`${state.name}: layout, hit areas, axe`, async ({ page, open }) => {
        await open(state.config, { clock: state.clock })
        await state.steps(page)
        await settle(page)

        await realTimers(page)
        const axe = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()
        const serious = axe.violations
          .filter((v) => v.impact === 'critical' || v.impact === 'serious')
          .map((v) => ({ id: v.id, impact: v.impact, help: v.helpUrl, nodes: v.nodes.map((n) => n.target.join(' ')) }))
        expect.soft(serious, 'axe critical and serious violations').toEqual([])

        const layout = await page.evaluate(layoutAudit)
        expect.soft(layout, JSON.stringify(layout, null, 1)).toEqual({ overflow: [], clipped: [], overlap: [], brokenImg: [] })

        // Last, since it scrolls each control to the middle.
        const hits = await page.evaluate(hitAudit)
        expect.soft(hits, JSON.stringify(hits, null, 1)).toEqual([])
      })
    }

    test('the end of a long result scrolls fully above the composer and its fade', async ({ page, open }) => {
      await open({ engine: 'running', mock: true })
      await page.getByRole('button', { name: /RSI reversion/ }).click()
      await page.getByRole('heading', { name: 'No trade', exact: true }).waitFor()
      const gap = await page.evaluate(() => {
        const scroller = document.querySelector('.lab-scroll')
        const composer = document.querySelector('.composer')
        const lab = document.querySelector('.lab')
        const last = document.querySelector('.footnote')
        if (!scroller || !composer || !lab || !last) return null
        scroller.scrollTop = scroller.scrollHeight
        const fade = parseFloat(getComputedStyle(lab).getPropertyValue('--composer-fade'))
        return composer.getBoundingClientRect().top - fade - last.getBoundingClientRect().bottom
      })
      expect(gap, 'px between the last line and the top of the fade').not.toBeNull()
      expect(gap).toBeGreaterThanOrEqual(0)
    })
  })
}
