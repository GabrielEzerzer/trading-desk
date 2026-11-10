import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    include: ['shared/src/**/*.test.ts', 'app/src/**/*.test.ts', 'app/mock/**/*.test.ts'],
    environment: 'node',
    testTimeout: 30_000,
  },
})
