# Third-party software

`orchd` vendors its web assets rather than fetching them at runtime, so a desktop
app never needs DNS to render its own labels. Everything under `web/vendor/` is
`include_str!`d into the binary, which means it is redistributed in both source
and binary form — so each item is listed here with the version it came from and
the notice its licence asks to travel with it.

Every licence below is compatible with this project's own
[AGPL-3.0-only](LICENSE).

Versions were recovered by hashing the vendored files against upstream releases,
not from a build record. Each file now carries its version in a header comment,
so the next person can answer "which one is this" without repeating that work.

## JavaScript and CSS

| File | Package | Version | Licence |
| --- | --- | --- | --- |
| `web/vendor/xterm.js` | [`@xterm/xterm`](https://github.com/xtermjs/xterm.js) | 6.0.0 | MIT |
| `web/vendor/xterm.css` | [`@xterm/xterm`](https://github.com/xtermjs/xterm.js) | 6.0.0 | MIT |
| `web/vendor/addon-fit.js` | [`@xterm/addon-fit`](https://github.com/xtermjs/xterm.js) | 0.11.0 | MIT |
| `web/vendor/addon-webgl.js` | [`@xterm/addon-webgl`](https://github.com/xtermjs/xterm.js) | 0.19.0 | MIT |
| `web/vendor/prism.min.js` | [`prismjs`](https://github.com/PrismJS/prism) | 1.29.0 | MIT |

The Prism build is the full-language download build — 230 grammars — because the
diff viewer highlights whatever the repository happens to contain.

### xterm.js, addon-fit, addon-webgl

```
Copyright (c) 2017-2019, The xterm.js authors (https://github.com/xtermjs/xterm.js)
Copyright (c) 2014-2016, SourceLair Private Company (https://www.sourcelair.com)
Copyright (c) 2012-2013, Christopher Jeffrey (https://github.com/chjj/)
```

### PrismJS

```
Copyright (c) 2012 Lea Verou
```

### The MIT licence, as it applies to all five files above

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Fonts

All four families are under the SIL Open Font License 1.1. The full text sits
beside each one in `web/vendor/fonts/`, carrying that family's own copyright and
Reserved Font Name, which is what the OFL asks for. Versions here are read from
each file's own `name` table.

| File | Family | Version | Licence |
| --- | --- | --- | --- |
| `plex-sans.woff2` | [IBM Plex Sans](https://github.com/IBM/plex) | 3.201 | OFL-1.1 — `ibm-plex-OFL.txt` |
| `plex-mono-400.woff2` | IBM Plex Mono | 2.3 | OFL-1.1 — `ibm-plex-OFL.txt` |
| `plex-mono-500.woff2` | IBM Plex Mono Medium | 2.3 | OFL-1.1 — `ibm-plex-OFL.txt` |
| `plex-mono-600.woff2` | IBM Plex Mono SemiBold | 2.3 | OFL-1.1 — `ibm-plex-OFL.txt` |
| `martian-mono.woff2` | [Martian Mono SemiExpanded](https://github.com/evilmartians/mono) | 1.000 | OFL-1.1 — `martian-mono-OFL.txt` |
| `jetbrains-mono-400.woff2` | [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) | 2.304 | OFL-1.1 — `jetbrains-mono-OFL.txt` |

All six are Latin-subset builds, so they are modified copies in the OFL's sense:
the outlines are upstream's, with the rest of the Unicode range dropped. The OFL
permits that and the family names are unchanged, which it also permits — the
Reserved Font Name clause bites on *renaming*, not on subsetting.

## Rust dependencies

Resolved through `Cargo.lock`; licences are each crate's own. The tree is MIT,
Apache-2.0, BSD, ISC, Zlib, BSL-1.0, Unicode-3.0, CC0 and Unlicense, plus five
MPL-2.0 crates reached through Tauri and `dirs` — `cssparser`,
`cssparser-macros`, `selectors`, `dtoa-short` and `option-ext`. MPL-2.0 §3.3
authorises combination under a GPL-family secondary licence, so all of it is
one-way compatible into AGPL-3.0.

On Linux the desktop shell links **WebKitGTK** (LGPL-2.1 and BSD) as a system
library. The package depends on `libwebkit2gtk-4.1-0` rather than bundling it,
which is what keeps the LGPL's relinking obligation off this project.

## Development-only tools

`tools/package.json` pulls `playwright-core` (Apache-2.0), `dependency-cruiser`
(MIT) and `typescript` (Apache-2.0). `node_modules/` is gitignored, the package
is `private`, and nothing under `tools/` is compiled into the binary — so none of
it is redistributed.
