// A `mise` that answers the three questions the daemon asks it, and records every
// call so a flow can assert what was run rather than that something was.
//
//   fake-mise.mjs <sandbox dir> <mise's own argv…>
//
// A file beside `fake-claude.mjs`, `fake-curl.mjs` and `fake-gh.mjs` rather than a
// string inside the flow: this one is linted, type-checked and runnable by hand
// like the other three, and the flow writes the same two-line shim the harness
// writes for them.
//
// **What the daemon asks**, in the order `update::check` asks it:
//
//   mise which claude          → where the agent came from; `tool_of_install_path`
//                                reads the tool out of mise's own layout
//   mise outdated --json <tool> → `{"<tool>": {"current": …, "latest": …}}`, and
//                                absent means "current", which is silence
//   mise upgrade <tool>        → the press itself
//
// `upgrade` fails while a `mise-fail` file exists in the sandbox, which is how a
// flow drives the failure half of the report without a second sandbox.
import fs from 'node:fs'
import path from 'node:path'

const [root, ...argv] = process.argv.slice(2)
const at = (name) => path.join(root, name)

// Appended, never truncated: a flow asserts on the *sequence* — three presses is
// what says each one reached the installer.
fs.appendFileSync(at('mise.log'), `${argv.join(' ')}\n`)

const [verb, ...rest] = argv
if (verb === 'which') {
  process.stdout.write(`${at('installs/claude-code/2.1.0/claude')}\n`)
} else if (verb === 'outdated') {
  const tool = rest[rest.length - 1]
  process.stdout.write(JSON.stringify({ [tool]: { current: '2.1.0', latest: '2.2.0' } }))
} else if (verb === 'upgrade' && fs.existsSync(at('mise-fail'))) {
  // stderr, because that is where the daemon reads a failure from: `mise` puts
  // progress on stdout, and a tail that took stdout would carry the noise and
  // lose the answer.
  process.stderr.write('mise: no such version 2.2.0\n')
  process.exit(1)
}
