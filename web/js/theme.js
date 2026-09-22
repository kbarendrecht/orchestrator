// The board's colours, fonts and sizes — everything about how it looks that is
// this browser's opinion rather than the daemon's.
//
// **Out of `core.js` because it is a feature, not a floor.** `core` is what every
// pane imports and must therefore hold only what every pane needs; 400 lines of
// palette solving, font detection and `localStorage` reading is a pane's worth of
// code that two modules use. It stayed there because it was in `app.js` first and
// `core` was where things went when the modules were cut.
//
// It imports `palette` and nothing else, which is what keeps the graph a DAG:
// `core` reads the theme for the terminal's colours, so a theme that reached back
// into `core` would be a cycle. The arithmetic stays in `palette`, which is pure so
// `check-palette.mjs` can import it in node and assert the default reproduces
// `app.css`'s `:root`.

import * as Palette from './palette.js';

/* ---------------------------------------------------------------------------
 * Theme
 * ------------------------------------------------------------------------- */

/* Three colours, three fonts and two numbers, in `localStorage` beside the zoom
 * and the column widths. Nothing about which colours you like belongs in
 * `config.json`, where a daemon that never reads it would have to carry it.
 *
 * The *arithmetic* is in `palette.js`, which is pure so `mise run check-web` can
 * import it in node and assert the default reproduces `app.css`'s `:root`. What
 * lives here is everything that touches the page: reading the store, refusing a
 * pair that cannot be read, writing the custom properties, and telling the
 * terminals. */

const THEME = { key: 'orch.theme' };

/** The vendored families, which are the only ones certain to be there.
 *
 *  `label` is what the dropdown shows and `stack` is what the token becomes. The
 *  `system` entry is the generic rather than a name: macOS does not expose its
 *  system faces by name — `ui-monospace` answers where `SF Mono` does not — so
 *  asking for the name would come back absent on the one platform that has it.
 */
export const FONTS = {
  plex: { label: 'IBM Plex Mono', stack: "'IBM Plex Mono',ui-monospace,monospace", mono: true },
  jetbrains: { label: 'JetBrains Mono', stack: "'JetBrains Mono',ui-monospace,monospace", mono: true },
  martian: { label: 'Martian Mono', stack: "'Martian Mono',ui-monospace,monospace", mono: true },
  system: { label: 'System monospace', stack: 'ui-monospace,monospace', mono: true },
  plexsans: { label: 'IBM Plex Sans', stack: "'IBM Plex Sans',system-ui,sans-serif", mono: false },
  sans: { label: 'System sans', stack: 'system-ui,sans-serif', mono: false },
};

/** What a fresh install gets: the palette in `app.css`, and the fonts it names. */
/* Declared above `theme` on purpose: `loadTheme` reads it, and a `const` is in its
   temporal dead zone until the line that defines it runs. With this below, the
   whole module threw on import — so the page loaded its markup and no behaviour at
   all, which looks like a dead board rather than an error. */
/** @typedef {'ui' | 'mono' | 'code'} Role */

const THEME_DEF = {
  ...Palette.DEFAULT,
  ui: 'plexsans',
  mono: 'plex',
  code: 'jetbrains',
  /** Terminal font size before the interface scale multiplies it — `term.js`'s old
   *  constant. */
  termSize: 12,
  /** Diff and code font size, the same way. 12 rather than the 11.5 the stylesheet
   *  used to hard-code: a size control has to show a whole number, and half a pixel
   *  is not a size anyone chose. */
  diffSize: 12,
  /** 1 is opaque. Floored well above zero: a board you cannot read is the problem
   *  transparency causes rather than the effect it is for. */
  opacity: 1,
  /** Which palette the terminals paint in. `board` is the one derived from the
   *  three colours above; every other value is a scheme taken whole, ground and
   *  all — see [`Palette.TERM_SCHEMES`] for why that is not mixed like the rest. */
  term: Palette.TERM_BOARD,
  /** Whether the colours are hand-tuned rather than a preset's.
   *
   *  **Stored, where the preset itself is derived.** [`currentPreset`] reads the
   *  colours back, which is right for "did I edit this away from Paper" and wrong
   *  for "I chose Custom": picking Custom changes nothing, so a derived answer
   *  would snap the dropdown back to whichever preset the board already matched
   *  and fold the wells away under the hand reaching for them. It is the one bit
   *  of the theme that is a decision rather than a colour. */
  custom: false,
};

/** Whole palettes, because one colour at a time cannot get you from dark to light.
 *
 *  **This is not a convenience.** Each change is judged against the other two, so
 *  walking a dark theme toward a light one is refused at every step: a white ground
 *  under light text is unreadable, and so is dark text on a dark ground. A preset
 *  moves all three at once, which is the only path between them — and it is how
 *  anybody switches anyway.
 *
 *  `orchd` is the palette the app ships with, so "Reset" is a real answer rather
 *  than something that resembles it; `check-palette.mjs` asserts that.
 *
 *  **Each carries its own opacity**, so a preset is the whole appearance of the
 *  board rather than three of its four parts. All three ship opaque, which is what
 *  they have always been — the field is what a hand-tuned theme varies, and
 *  picking a preset is how that is put back.
 */
export const PRESETS = {
  orchd: { label: 'orchd', ...Palette.DEFAULT, opacity: 1 },
  paper: { label: 'Paper', bg: '#F4F2ED', panel: '#EAE7E0', text: '#26231E', opacity: 1 },
  contrast: { label: 'High contrast', bg: '#000000', panel: '#0C0C0C', text: '#FFFFFF', opacity: 1 },
};

/** Which preset the board is on, or `null` for a hand-tuned set.
 *
 *  Derived from the values rather than stored, so a theme nudged off a preset and
 *  back reads as that preset again. Compared lowercase: an `input[type=color]`
 *  always reports lowercase, and the constants above are written the way a person
 *  writes them.
 *
 *  **`theme.custom` overrides the derivation, and opacity is part of the match.**
 *  The flag is there because picking Custom changes no value — see `THEME_DEF`.
 *  Opacity counts because a preset now carries one: a board at 72% is not Paper,
 *  and a dropdown still saying Paper would be the pane disagreeing with the
 *  window.
 */
export function currentPreset() {
  if (theme.custom) return null;
  const same = (/** @type {string} */ a, /** @type {string} */ b) => a.toLowerCase() === b.toLowerCase();
  const roles = /** @type {const} */ (['bg', 'panel', 'text']);
  return Object.keys(PRESETS).find((k) => {
    const p = PRESETS[/** @type {keyof typeof PRESETS} */ (k)];
    return roles.every((role) => same(p[role], theme[role])) && p.opacity === theme.opacity;
  }) ?? null;
}

/* Monospace and sans families worth *asking* about.
 *
 * **A list, because a page cannot enumerate installed fonts here.**
 * `queryLocalFonts()` is the API for that, and it is Chromium-only behind a
 * permission prompt — absent from WebKit, so absent from WKWebView on macOS and
 * from WebKitGTK on Linux, which is every window this app opens. Asking whether
 * one named family resolves does work everywhere.
 *
 * Notably **not** `SF Mono`: macOS does not expose its system faces by name, and
 * the generic answers where the name does not — which is why `system` is in
 * [`FONTS`] as `ui-monospace` rather than as a name that comes back absent. */
const MONO_CANDIDATES = [
  'Berkeley Mono', 'Cascadia Code', 'Cascadia Mono', 'Comic Mono', 'Consolas',
  'Courier New', 'DejaVu Sans Mono', 'Fira Code', 'Fira Mono', 'Geist Mono',
  'Hack', 'Iosevka', 'Inconsolata', 'Liberation Mono', 'Menlo', 'Monaco',
  'MonoLisa', 'Noto Sans Mono', 'Roboto Mono', 'Source Code Pro',
  'SF Mono Powerline', 'Ubuntu Mono', 'Victor Mono', 'Zed Mono',
];
const SANS_CANDIDATES = [
  'Arial', 'Avenir Next', 'DejaVu Sans', 'Helvetica Neue', 'Inter', 'Lato',
  'Noto Sans', 'Open Sans', 'Roboto', 'Segoe UI', 'Source Sans 3', 'Ubuntu',
];

/** Whether asking for `name` gets you anything other than the default face.
 *
 *  **It cannot tell an installed family from an aliased one, and that is a real
 *  limit rather than a bug to fix.** fontconfig — WebKitGTK, the Linux target —
 *  substitutes by design: `Courier New` resolves to Liberation Mono on a machine
 *  that has never had it, and nothing the page can measure sees the difference.
 *  The branch this comes from probed against three generics and called agreement
 *  proof, which is a Chrome-shaped assumption: under fontconfig all three agree
 *  *because* the alias resolves the same way regardless of what follows it.
 *
 *  So this asks the narrower question it can actually answer — does this name
 *  resolve to something other than the fallback — against a family that certainly
 *  does not exist. One comparison rather than three, and immune to the agreement
 *  trap. **The preview beside each control is what makes the remaining error
 *  harmless**: you see the face you will get before you keep it.
 */
function resolves(/** @type {string} */ name, /** @type {CanvasRenderingContext2D} */ ctx) {
  const NOTHING = '__orchd_no_such_family__';
  const sample = 'MWil10O—mmmiii';
  const width = (/** @type {string} */ family) => {
    ctx.font = `48px ${family}`;
    return ctx.measureText(sample).width;
  };
  return width(`'${NOTHING}'`) !== width(`'${name}','${NOTHING}'`);
}

/** The candidate families that resolve here, by role. Measured once, lazily.
 *
 *  **Not at module scope.** Two lists of measurements on the boot path lengthens
 *  the near-black window before the first paint, for a list nothing reads until
 *  somebody opens the settings pane. The branch this comes from did it at import.
 */
/** @type {{ mono: string[], sans: string[] } | null} */
let detected = null;
export function detectedFonts() {
  if (detected) return detected;
  const ctx = document.createElement('canvas').getContext('2d');
  if (!ctx) return { mono: [], sans: [] };
  const shipped = new Set(Object.values(FONTS).map((f) => f.label));
  const find = (/** @type {string[]} */ names) => names.filter((/** @type {string} */ n) => !shipped.has(n) && resolves(n, ctx));
  detected = { mono: find(MONO_CANDIDATES), sans: find(SANS_CANDIDATES) };
  return detected;
}

/** The theme as it stands. Replaced whole by [`setTheme`], never mutated. */
/** 8 to 24 px, and not `NaN`. The floor is where a terminal stops being one.
 *
 *  **Above `loadTheme()`'s call, and that is load-bearing.** `loadTheme` runs at
 *  module load and calls `clampSize`, which reads these — so declared below that
 *  line they are in the temporal dead zone and the read throws
 *  `Cannot access 'SIZE_MAX' before initialization`, which takes the whole SPA
 *  down before it paints. It hid for as long as it did because the unset path
 *  never reaches them: `Number(undefined)` is `NaN`, so `clampSize` returns the
 *  default without evaluating either. Store a font size once — the settings pane
 *  writes one — and the next load is a blank page. Found by a gate that picked a
 *  theme and reloaded. */
export const SIZE_MIN = 8;
export const SIZE_MAX = 24;

export let theme = loadTheme();

/** @type {((theme: Theme) => void)[]} */
const themeListeners = [];
/** Register for theme changes. The terminals are the one consumer that cannot
 *  read a CSS custom property — xterm takes hex strings — so they are told. */
export function onThemeChange(/** @type {(theme: Theme) => void} */ fn) { themeListeners.push(fn); }

/** Read the stored theme, keeping only what is valid.
 *
 *  **Field by field, and normalised on the way in.** A stored `"D2D2D2"` passes a
 *  tolerant hex test and is then not a colour: written to a custom property it
 *  kills every rule that reads it, and the colour well shows `#000000` while the
 *  board says otherwise. So what comes back is what `parseHex` accepted, spelled
 *  `#rrggbb`.
 *
 *  A font key is checked with `Object.hasOwn`, not `FONTS[key]` — `"constructor"`
 *  and `"toString"` pass the latter, and the token then becomes the literal string
 *  `undefined`.
 */
/** The board's theme: three colours, three font families, two sizes and the
 *  window opacity. Spelled out because the settings pane indexes it by a role
 *  name, and a checker with no shape to index cannot tell `theme.termSize` from
 *  a typo.
 *
 *  @typedef {{ bg: string, panel: string, text: string,
 *              ui: string, mono: string, code: string,
 *              termSize: number, diffSize: number, opacity: number,
 *              term: string, custom: boolean }} Theme
 */

/** @returns {Theme} */
function loadTheme() {
  /** @type {Record<string, unknown>} */
  let got = {};
  try {
    got = JSON.parse(localStorage.getItem(THEME.key) || '{}') || {};
  } catch (e) {
    got = {};
  }
  const hex = (/** @type {unknown} */ v, /** @type {string} */ fallback) => {
    const rgb = Palette.parseHex(/** @type {string | null | undefined} */ (v));
    return rgb ? Palette.toHex(rgb) : fallback;
  };
  const family = (/** @type {unknown} */ v, /** @type {string} */ fallback) =>
    (typeof v === 'string' && (v.startsWith('custom:') || Object.hasOwn(FONTS, v)) ? v : fallback);
  const next = {
    bg: hex(got.bg, THEME_DEF.bg),
    panel: hex(got.panel, THEME_DEF.panel),
    text: hex(got.text, THEME_DEF.text),
    ui: family(got.ui, THEME_DEF.ui),
    mono: family(got.mono, THEME_DEF.mono),
    code: family(got.code, THEME_DEF.code),
    termSize: clampSize(got.termSize, THEME_DEF.termSize),
    diffSize: clampSize(got.diffSize, THEME_DEF.diffSize),
    opacity: clampOpacity(got.opacity),
    term: Palette.validScheme(got.term) ? String(got.term) : THEME_DEF.term,
    /* `=== true` rather than a cast: this is the one field the store can hold a
       string or a number in and mean nothing by it, and a truthy `"false"` would
       unlock the colour wells on a board nobody hand-tuned. */
    custom: got.custom === true,
  };
  /* A pair that cannot be read never reaches the page, however it got into the
     store — a hand edit, or a build that once allowed it. Falling back to the
     default is the only recovery that does not need a readable settings pane to
     reach. */
  return Palette.legible(next) ? next : { ...next, ...Palette.DEFAULT };
}

function clampSize(/** @type {unknown} */ v, /** @type {number} */ def) {
  const n = Number(v);
  return Number.isFinite(n) ? Math.min(SIZE_MAX, Math.max(SIZE_MIN, Math.round(n))) : def;
}

/** 0.35 to 1. Floored well above zero for the reason `THEME_DEF.opacity` gives. */
function clampOpacity(/** @type {unknown} */ v) {
  const n = Number(v);
  if (!Number.isFinite(n)) return 1;
  return Math.min(1, Math.max(0.35, Math.round(n * 100) / 100));
}

/** Whether a family name is one this app will put in a declaration.
 *
 *  **Refused rather than mangled**, and one spelling so the pane and the stack
 *  cannot disagree about what is allowed. Deleting the characters that could break
 *  a declaration corrupts legitimate names, and what survives can still inject a
 *  second family — `Comic, monospace` is two. Letters, digits, spaces, dots and
 *  hyphens cover every real family name and nothing that can end a declaration.
 */
export const validFontName = (/** @type {string | null | undefined} */ name) => /^[\w .-]{1,64}$/.test(name ?? '');

/** The CSS stack for one role, or the vendored default if the key is unknown. */
export function fontStack(/** @type {Role} */ role) {
  const key = theme[role];
  if (typeof key === 'string' && key.startsWith('custom:')) {
    /* A name that does not pass falls back to the vendored stack rather than
       being cleaned up — see `validFontName`. The pane refuses it before it gets
       here; this is the second line of defence for a hand-edited store. */
    const name = key.slice('custom:'.length);
    if (validFontName(name)) {
      const generic = role === 'ui' ? 'system-ui,sans-serif' : 'ui-monospace,monospace';
      return `'${name}',${generic}`;
    }
  }
  return (FONTS[/** @type {keyof typeof FONTS} */ (key)]
    || FONTS[/** @type {keyof typeof FONTS} */ (THEME_DEF[role])]).stack;
}

/** Write the theme to the page.
 *
 *  Everything derived goes on `documentElement` as a custom property, so the
 *  stylesheet keeps saying `var(--line)` and knows nothing about themes. The
 *  semantic colours are set here too — with their hue kept and their luminance
 *  lifted only where the ground would swallow them; `Palette.readable` has why.
 */
function applyTheme() {
  const root = document.documentElement;
  const board = Palette.tokens(theme, { opacity: theme.opacity });
  for (const [name, value] of Object.entries(board)) {
    root.style.setProperty(name, value);
  }
  for (const [name, hue] of Object.entries(SIGNALS)) {
    root.style.setProperty(name, Palette.readable(hue, theme));
  }
  root.style.setProperty('--sans', fontStack('ui'));
  root.style.setProperty('--label', fontStack('ui'));
  root.style.setProperty('--mono', fontStack('mono'));
  root.style.setProperty('--code', fontStack('code'));
  /* **What the box around a terminal is painted.** xterm paints its own ground and
     nothing else, so the 8px inset `.termhost` holds would stay the *board's*
     colour under a scheme that brought its own — a frame two shades off the pane
     it surrounds, which reads as a bug rather than as a choice. A scheme's ground
     is opaque, which is what the terminal already was: `allowTransparency` reaches
     xterm's DOM renderer and not its WebGL one (see `Palette.termColours`). */
  const scheme = Palette.TERM_SCHEMES[/** @type {keyof typeof Palette.TERM_SCHEMES} */ (theme.term)];
  root.style.setProperty('--term-ground', scheme ? scheme.background : board['--ground']);
  /* The diff's own size, before `--fs` multiplies it — the stylesheet does that
     multiplication, so the three code blocks that share this size keep sharing it. */
  root.style.setProperty('--code-px', `${theme.diffSize}px`);
  /* **What the engine paints a `<select>`, a scrollbar and a range track.** Those
     are the browser's own widgets, and without this it draws them for a light page
     whatever the stylesheet says — so the settings pane's dropdowns came up white on
     a black board under WebKitGTK, along with the list each one opens, which no CSS
     of ours can reach at all. Read from the palette rather than stored: a theme
     whose ground is darker than its text is a dark theme, and that is true of a
     preset and of a hand-edited pair alike. */
  root.style.colorScheme =
    Palette.luminance(theme.bg) < Palette.luminance(theme.text) ? 'dark' : 'light';
  for (const fn of themeListeners) fn(theme);
}

/* The colours that mean something, with their shipped hues.
 *
 * Out of `tokens()` on purpose: these are the legend three panes read, and a
 * theme that could set amber to grey would be turning a signal off rather than
 * restyling it. Their *luminance* still follows the theme — see `Palette.readable`
 * for the light-ground failure that forced it. */
const SIGNALS = {
  '--attn': '#E0A244',
  '--work': '#4C9AAF',
  '--ok': '#5FA97C',
  '--bad': '#D4726B',
  '--auto': '#5B8FC9',
  '--focus': '#C9C9C9',
};

/** Change part of the theme, or refuse.
 *
 *  Returns `null` on success and a sentence on refusal, so the pane can say why
 *  rather than snapping a control back with no explanation.
 *
 *  **A pair below the floor is refused, not corrected.** A board is the colours
 *  you picked, and quietly moving them is the worse answer — and the refusal is
 *  what keeps the way back reachable, since a theme that made the settings pane
 *  invisible could only be undone by clearing browser storage.
 */
export function setTheme(/** @type {Partial<Theme>} */ patch) {
  const next = { ...theme, ...patch };
  if (!Palette.legible(next)) {
    const got = Palette.contrast(next.bg, next.text).toFixed(1);
    return `Text on that ground is ${got}:1 — under ${Palette.MIN_CONTRAST}:1 the board `
      + 'stops being readable, so this is not applied.';
  }
  theme = {
    ...next,
    termSize: clampSize(next.termSize, THEME_DEF.termSize),
    diffSize: clampSize(next.diffSize, THEME_DEF.diffSize),
    opacity: clampOpacity(next.opacity),
    term: Palette.validScheme(next.term) ? next.term : THEME_DEF.term,
    custom: next.custom === true,
  };
  try {
    localStorage.setItem(THEME.key, JSON.stringify(theme));
  } catch (e) { /* private mode: the theme still holds for this session */ }
  applyTheme();
  return null;
}

/** Back to the palette the app shipped with — which `check-palette.mjs` asserts is
 *  exactly what `:root` declares, so this is a real answer rather than one that
 *  resembles it. */
export function resetTheme() {
  return setTheme(THEME_DEF);
}


/* Applied at module scope, which is the earliest the page can be themed: modules
   are deferred, so `documentElement` is there, and this runs before `app.js` has
   rendered anything. Any later and the board paints `:root`'s palette first and
   then visibly changes colour — which is the defect the ratio solving removes for
   the *default* theme and cannot remove for anybody else's. */
applyTheme();

/** Whether the window behind this page is see-through.
 *
 *  Told by the host, which read it out of `host.json` when it built the window —
 *  so the page never has to guess whether lowering the opacity will show the
 *  desktop or nothing at all. False in a browser tab, where the tab's own ground
 *  is behind the page.
 */
export const SEE_THROUGH = window.__ORCH__.seeThrough === true;
