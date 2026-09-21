#!/bin/sh
# The SPA's gate, in one place because it had three.
#
# `mise run check-web`, `check.yml`'s "Check the SPA" step and the pre-commit hook
# each spelled this list themselves, and the hook's copy had already lost three
# entries — the palette, the dead CSS and the drop lists. A list that is written
# down twice is a list that agrees until somebody adds the fourth check to one
# copy. The hook still runs a subset on purpose (it runs only what the staged
# files could break), so what is unified here is the two that claim to be the
# whole gate.
#
# Run it from anywhere; it needs `tools/node_modules` (`mise run deps`).
set -e
cd "$(dirname "$0")/.."

# Top of the graph down, and the order is load-bearing. `ts-rs` exports a type's
# *dependencies* too, so `orchd-repo`'s run rewrites `base.d.ts` with only the one
# base type it happens to reference — 15 types down to 1. Running each crate after
# everything that depends on it means each file is written last by the crate that
# owns it. Measured, after `--workspace` produced exactly that truncation.
for crate in orchd-serve orchd orchd-repo orchd-base; do
  cargo test -p "$crate" --lib export_bindings >/dev/null
done
git diff --exit-code --stat web/snapshot.d.ts web/base.d.ts web/repo.d.ts web/serve.d.ts \
  || { echo 'the generated types were stale — the regenerated copies are in your tree; commit them.'; exit 1; }

npx --prefix tools tsc -p tools/tsconfig.json
npx --prefix tools depcruise --config tools/.dependency-cruiser.cjs web/app.js
npx --prefix tools eslint --config tools/eslint.config.mjs web
node tools/check-palette.mjs
node tools/check-dead-css.mjs
node tools/check-drop-lists.mjs
node tools/check-module-routes.mjs
node tools/check-pathlink.mjs
node tools/check-markdown.mjs
