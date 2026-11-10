// Process helpers for the scripts that launch the built app: the tree under a pid, and a wait
// for every pid in it to be gone.

import { execFileSync } from 'node:child_process'

function table() {
  if (process.platform === 'win32') {
    const csv = execFileSync(
      'powershell.exe',
      ['-NoProfile', '-Command', 'Get-CimInstance Win32_Process | ForEach-Object { "$($_.ProcessId),$($_.ParentProcessId),$($_.Name)" }'],
      { encoding: 'utf8' },
    )
    return csv
      .split(/\r?\n/)
      .filter(Boolean)
      .map((line) => {
        const [pid, ppid, ...name] = line.split(',')
        return { pid: Number(pid), ppid: Number(ppid), name: name.join(',') }
      })
  }
  const out = execFileSync('ps', ['-eo', 'pid=,ppid=,comm='], { encoding: 'utf8' })
  return out
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      const [pid, ppid, ...name] = line.trim().split(/\s+/)
      return { pid: Number(pid), ppid: Number(ppid), name: name.join(' ') }
    })
}

/** The process and all its descendants, as { pid, name }. */
export function processTree(rootPid) {
  const rows = table()
  const found = rows.filter((r) => r.pid === rootPid)
  for (let i = 0; i < found.length; i++) {
    for (const row of rows) if (row.ppid === found[i].pid && !found.includes(row)) found.push(row)
  }
  return found.map(({ pid, name }) => ({ pid, name }))
}

/** Waits until none of the pids is alive; returns the ones still alive at the deadline. */
export async function waitGone(pids, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const alive = new Set(table().map((r) => r.pid))
    const left = pids.filter((p) => alive.has(p.pid))
    if (left.length === 0 || Date.now() > deadline) return left
    await new Promise((r) => setTimeout(r, 250))
  }
}

export function killAll(pids) {
  for (const { pid } of pids) {
    try {
      process.kill(pid)
    } catch {}
  }
}
