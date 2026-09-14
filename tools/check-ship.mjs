#!/usr/bin/env node
// Every binary the workspace builds reaches every way of installing it.
//
//   mise run check-ship
//
// **This exists because v2026.9.14 shipped without `orchd` (#16).** The app had
// just been split into a host and a child daemon — `child.rs::daemon_binary`
// resolves `orchd` beside the running executable — while the release job still
// built `--bin orch` and the three bundle maps still copied `orch` alone. Every
// install method was affected, the tarball included, and nothing anywhere failed:
// `cargo test`, clippy, the SPA checks and 25 e2e flows were all green, because
// the fault was in what got *packed*, which no test in this repo could see. The
// first sign was a user on a fresh install: "Orchestrator could not start".
//
// So the rule is derived rather than listed. `cargo metadata` says what binaries
// the workspace produces; everything but the app itself must be built by the
// release job, packed into the tarball, and copied by each bundle's `files` map.
// Add a fourth binary and this fails until it is placed, which is the whole point:
// the list nobody remembered to update is not written down anywhere any more.
//
// The one binary deliberately absent from the `files` maps is the app. The Tauri
// bundler places it, and naming it there would put a second copy inside every
// bundle.

import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const read = (rel) => fs.readFileSync(path.join(repoRoot, rel), 'utf8')

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}

// --- what the workspace builds ----------------------------------------------
//
// Asked of cargo rather than parsed out of the manifests: a `[[bin]]` can be
// implicit (`src/main.rs` with no stanza at all), and a rule that only sees the
// explicit ones would miss exactly the binary somebody added in a hurry.
const meta = JSON.parse(
  execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'],
    { cwd: repoRoot, encoding: 'utf8', maxBuffer: 64 << 20 }),
)

/** Every bin target, with the package that owns it and whether it is the app. */
const bins = meta.packages.flatMap((p) =>
  p.targets
    .filter((t) => t.kind.includes('bin'))
    .map((t) => ({
      name: t.name,
      pkg: p.name,
      // The desktop app is the one the Tauri bundler places itself. Identified by
      // where its manifest sits, not by its name, so renaming the product does not
      // quietly turn this check off.
      isApp: path.relative(repoRoot, p.manifest_path).startsWith('desktop' + path.sep),
    })),
)
const shipped = bins.filter((b) => !b.isApp).map((b) => b.name).sort()
const app = bins.find((b) => b.isApp)

check(bins.length > 0, `cargo reports ${bins.length} binaries`)
check(!!app, 'one of them is the desktop app')
check(shipped.length > 0, `the app ships alongside: ${shipped.join(', ')}`)

// --- the release job ---------------------------------------------------------
const release = read('.github/workflows/release.yml')

/* Built before the bundler runs, because each bundle's `files` map copies a path
   under `target/release` and the bundler does not build it. `--bins` on the
   owning package counts, and is what the job uses — a named list there is the
   same rot this check exists to stop. */
for (const b of shipped) {
  const pkg = bins.find((x) => x.name === b).pkg
  const built = new RegExp(
    `cargo build --release[^\\n]*(-p ${pkg}[^\\n]*--bins|--bin ${b}\\b)`,
  ).test(release)
  check(built, `release.yml builds ${b}`)
}

/* The tarball, which is what `mise` and `ubi` install: they read a release asset
   and extract by exe name, and neither can open a .deb. */
const tar = release.match(/tar -C target\/release[\s\S]*?\n\n/)?.[0] ?? ''
for (const b of [app.name, ...shipped]) {
  check(new RegExp(`\\b${b}\\b`).test(tar), `the tarball carries ${b}`)
}

// --- the bundles -------------------------------------------------------------
const tauri = JSON.parse(read('desktop/tauri.conf.json'))
const maps = {
  deb: tauri.bundle?.linux?.deb?.files,
  appimage: tauri.bundle?.linux?.appimage?.files,
  macOS: tauri.bundle?.macOS?.files,
}

for (const [which, files] of Object.entries(maps)) {
  if (!files) {
    check(false, `${which} has a files map`)
    continue
  }
  const entries = Object.entries(files)
  const names = entries.map(([dest]) => path.basename(dest)).sort()
  check(
    JSON.stringify(names) === JSON.stringify(shipped),
    `${which} copies exactly ${shipped.join(', ')}${
      JSON.stringify(names) === JSON.stringify(shipped) ? '' : ` (found ${names.join(', ') || 'nothing'})`}`,
  )

  /* **Beside the app, not merely present.** `daemon_binary` looks in
     `current_exe().parent()` and nowhere else, so a bundle that put `orchd` in a
     directory of its own would install a file the app still cannot find — a
     failure that looks exactly like the one this check is named after. Every
     entry in one map must therefore share one destination directory. */
  const dirs = [...new Set(entries.map(([dest]) => path.dirname(dest)))]
  check(dirs.length === 1, `${which} puts them all in one directory (${dirs.join(', ')})`)

  // And each source is the release build's own output, which is what the build
  // step above was checked to produce.
  for (const [dest, src] of entries) {
    check(src === `../target/release/${path.basename(dest)}`,
      `${which}: ${dest} comes from the release build (${src})`)
  }
}

console.log(`\ncheck-ship: ${failed ? 'FAILED' : 'ok'}`)
process.exit(failed ? 1 : 0)
