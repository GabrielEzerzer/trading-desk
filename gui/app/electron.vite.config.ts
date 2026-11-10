import react from '@vitejs/plugin-react'
import { defineConfig } from 'electron-vite'
import type { Plugin } from 'vite'

// The production page loads only its own bundled files and talks to nothing: every engine call
// goes through main over IPC. The dev server needs inline scripts (React refresh) and its own
// WebSocket for hot reload, so dev gets a looser policy.
const PROD_CSP = [
  "default-src 'none'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self' data:",
  "font-src 'self'",
  "connect-src 'none'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join('; ')

const DEV_CSP = [
  "default-src 'self'",
  "script-src 'self' 'unsafe-inline'",
  "style-src 'self' 'unsafe-inline'",
  "img-src 'self' data:",
  "connect-src 'self' ws://localhost:* ws://127.0.0.1:*",
  "object-src 'none'",
  "base-uri 'none'",
].join('; ')

function contentSecurityPolicy(): Plugin {
  return {
    name: 'td-csp',
    transformIndexHtml(_html, context) {
      return [
        {
          tag: 'meta',
          attrs: { 'http-equiv': 'Content-Security-Policy', content: context.server ? DEV_CSP : PROD_CSP },
          injectTo: 'head-prepend',
        },
      ]
    },
  }
}

// Everything the app uses is a devDependency, so main and preload are bundled whole; a sandboxed
// preload cannot load other modules at all.
export default defineConfig({
  main: {},
  preload: {},
  renderer: {
    plugins: [react(), contentSecurityPolicy()],
  },
})
