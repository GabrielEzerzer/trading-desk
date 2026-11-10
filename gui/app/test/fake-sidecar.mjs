// Stands in for td-server in the sidecar tests. The first argument picks how it behaves:
//   ok        prints a handshake and exits when stdin closes, like the real server
//   early     writes to stderr and exits with code 3 before any handshake
//   silent    never prints a handshake and keeps running
//   stubborn  prints a handshake and ignores stdin closing
//   garbage   prints a line that is not a handshake
const mode = process.argv[2]
const handshake = JSON.stringify({ port: 45678, token: 'f'.repeat(64) })
const keepAlive = () => setInterval(() => {}, 60_000)

switch (mode) {
  case 'ok':
    process.stdout.write(handshake + '\n')
    process.stderr.write('fake: listening\n')
    process.stdin.on('end', () => process.exit(0))
    process.stdin.resume()
    break
  case 'early':
    process.stderr.write('fake: cannot open the data directory\n', () => process.exit(3))
    break
  case 'silent':
    keepAlive()
    break
  case 'stubborn':
    process.stdout.write(handshake + '\n')
    keepAlive()
    break
  case 'garbage':
    process.stdout.write('hello\n')
    keepAlive()
    break
  default:
    process.stderr.write(`fake: unknown mode ${mode}\n`)
    process.exit(2)
}
