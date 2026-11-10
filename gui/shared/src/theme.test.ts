import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'
import { GROUND, INK, TITLE_BAR_HEIGHT } from './theme'

const css = readFileSync(new URL('./theme.css', import.meta.url), 'utf8')

function token(name: string): string | undefined {
  return new RegExp(`--${name}:\\s*([^;]+);`).exec(css)?.[1]?.trim()
}

describe('theme', () => {
  it('gives main the same values as theme.css', () => {
    expect(token('ground')).toBe(GROUND)
    expect(token('ink')).toBe(INK)
    expect(token('titlebar-height')).toBe(`${TITLE_BAR_HEIGHT}px`)
  })
})
