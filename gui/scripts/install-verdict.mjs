// How the install check reads the installed app's one launch (install-check.mjs). The app passes
// when it reported TD_SMOKE ok, started the engine from the install folder on the fresh data folder,
// and the engine stopped cleanly. Exit code 0 is a plain pass. Exit code 1 after all of that is the
// quit watchdog ending a slow Chromium shutdown (watchdog.ts; #71 owns that), so it passes with a
// note that keeps the slow quit visible. The smoke's own failure exits with 2 (smoke.ts).

export const WATCHDOG_NOTE = ' (the quit watchdog ended the app after the engine stopped cleanly; see #71)'

/**
 * Returns the suffix for the pass line: '' or WATCHDOG_NOTE. Throws with the reason on a failure.
 * `code` is the exit code, or 'timeout' if the app was still running at the limit.
 */
export function launchVerdict({ code, output, engineBin, dataDir }) {
  if (code === 'timeout') throw new Error('the app was still running at the time limit')
  const fail = output.find((line) => line.startsWith('TD_SMOKE fail:'))
  if (fail) throw new Error(`${fail} (exit code ${code})`)
  const ok = output.find((line) => line.startsWith('TD_SMOKE ok engine='))
  if (!ok) throw new Error(`the app exited with code ${code} without reporting the smoke result`)
  const ran = ok.slice('TD_SMOKE ok engine='.length)
  if (ran.toLowerCase() !== engineBin.toLowerCase()) throw new Error(`the app started the engine at ${ran}, not ${engineBin}`)
  if (!output.some((line) => line.includes('td-server: listening on') && line.includes(dataDir))) {
    throw new Error(`the engine did not report the fresh data folder ${dataDir}`)
  }
  if (!output.includes('[td-server] stopped (clean)')) throw new Error(`the engine did not stop cleanly (exit code ${code})`)
  if (code === 0) return ''
  if (code === 1) return WATCHDOG_NOTE
  throw new Error(`the app exited with code ${code}`)
}
