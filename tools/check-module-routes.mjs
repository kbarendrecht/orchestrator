// Every SPA module must be served, and everything served must exist.
//
// The page is compiled in, so adding a file to `web/js/` is a *Rust* change: it
// needs an arm in `module()` in `src/host.rs` or the browser gets a 404 for an
// import that resolves perfectly on disk. Nothing said so until now.
// `dependency-cruiser` answers "is this module imported"; it cannot answer "is
// this module reachable", and the two failures look nothing alike — an
// unimported module is dead, an unserved one is a page that stops booting.
//
// The other direction is the cheaper half: an arm naming a file that was
// renamed does not compile, so `include_str!` already guards it. The arm *key*
// is what can drift, because `"trem.js" => include_str!("../web/js/term.js")`
// is a valid program.

import { readdirSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const onDisk = readdirSync(`${root}/web/js`).filter((f) => f.endsWith('.js')).sort();

// The arms of `module()` alone: `vendor()` below it has the same shape, and its
// files are not ours.
const host = readFileSync(`${root}/src/host.rs`, 'utf8');
const body = host.match(/async fn module\([\s\S]*?\n}/);
if (!body) {
  console.error('check-module-routes: no `async fn module(` in src/host.rs — has it moved?');
  process.exit(1);
}
const served = [...body[0].matchAll(/"([\w.-]+\.js)" => include_str!\("\.\.\/web\/js\/([\w.-]+\.js)"\)/g)];

const problems = [];
for (const [, key, file] of served) {
  if (key !== file) problems.push(`src/host.rs serves "${key}" from web/js/${file} — the names disagree`);
}
const keys = new Set(served.map(([, key]) => key));
for (const f of onDisk) {
  if (!keys.has(f)) problems.push(`web/js/${f} has no arm in module() in src/host.rs — it would 404`);
}
for (const key of keys) {
  if (!onDisk.includes(key)) problems.push(`src/host.rs serves "${key}", which is not in web/js/`);
}

if (problems.length) {
  for (const p of problems) console.error(`check-module-routes: ${p}`);
  process.exit(1);
}
console.log(`check-module-routes: ${onDisk.length} modules, all served`);
