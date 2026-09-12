// The colour maths behind the theme: three inputs, every other token mixed.
//
// **Pure on purpose — no DOM, no `window`, no `localStorage`.** That is what lets
// `tools/check-palette.mjs` import it in node and assert, at check time, that the
// default theme reproduces the `:root` block in `app.css` exactly. Without that
// the stylesheet and this file are two hand-maintained copies of one palette, and
// the review of the branch this came from found them already four shades apart —
// so "Reset" did not restore the palette the app shipped with, and the board
// visibly shifted between first paint and the theme settling.
//
// **Three colours, not twenty.** Ground, panel and text; everything else is a
// position between them. Twenty pickers reliably produce an unreadable board, and
// the interesting failure is not that somebody picks an ugly pair — it is that a
// *border* ends up the same value as the surface it separates, which no amount of
// care at the picker prevents. A ratio cannot do that.
//
// The ratios are not round numbers. They are solved from the palette somebody
// hand-tuned over months: `--raised` sits 0.05348 of the way from the panel to the
// text because that is where `#212121` is, given `#171717` and `#D2D2D2`. Keeping
// them exact is what makes the default a real answer rather than something that
// resembles it.

/** `#rrggbb` or `rrggbb` to `[r, g, b]`, or `null` if it is neither.
 *
 *  Tolerant of the missing `#` because people paste hex without it, but the
 *  caller must store what comes *back* — the branch this replaces validated the
 *  loose form and then wrote the raw string into a custom property, where
 *  `D2D2D2` is not a colour and every rule using it silently died.
 */
/** The three colours a board is built from; everything else is mixed out of them.
 *  @typedef {{ bg: string, panel: string, text: string }} Palette */

export function parseHex(/** @type {string | null | undefined} */ s) {
  const m = /^#?([0-9a-f]{6})$/i.exec(String(s ?? '').trim());
  if (!m) return null;
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/** Three channels back to `#rrggbb`, clamped and rounded. */
export function toHex(/** @type {number[]} */ rgb) {
  const out = rgb.map((/** @type {number} */ c) => Math.max(0, Math.min(255, Math.round(c))));
  return `#${out.map((/** @type {number} */ c) => c.toString(16).padStart(2, '0')).join('')}`;
}

/** `a` moved `t` of the way toward `b`. `t` of 0 is `a`, 1 is `b`.
 *
 *  Plain per-channel interpolation in sRGB rather than anything perceptual: the
 *  ratios below were solved against *this* arithmetic, so a cleverer mix would
 *  stop reproducing the palette it is calibrated on.
 */
export function mix(/** @type {string} */ a, /** @type {string} */ b, /** @type {number} */ t) {
  const [x, y] = [parseHex(a), parseHex(b)];
  if (!x || !y) return a;
  return toHex(x.map((c, i) => c + (y[i] - c) * t));
}

/** WCAG relative luminance. */
export function luminance(/** @type {string} */ hex) {
  const rgb = parseHex(hex);
  if (!rgb) return 0;
  const [r, g, b] = rgb.map((c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** WCAG contrast ratio, 1 to 21. Order does not matter. */
export function contrast(/** @type {string} */ a, /** @type {string} */ b) {
  const [x, y] = [luminance(a), luminance(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

/** The floor a text/ground pair has to clear.
 *
 *  WCAG AA for body text. It is applied to the *chosen* pair rather than to every
 *  derived token, because the derived ones are positions between two colours that
 *  already clear it — and to the semantic set, which is text as often as it is a
 *  dot.
 */
export const MIN_CONTRAST = 4.5;

/** The palette the app ships with, and what "Reset" restores.
 *
 *  These three are the *inputs*; `tokens()` turns them back into the thirteen
 *  values `app.css` declares. `tools/check-palette.mjs` asserts that, so the two
 *  cannot drift.
 */
export const DEFAULT = { bg: '#101010', panel: '#171717', text: '#D2D2D2' };

/* Where each token sits between two of the three inputs.
 *
 * Solved from the shipped palette rather than chosen: with `bg #101010`,
 * `panel #171717` and `text #D2D2D2`, these ratios land on exactly the values
 * `:root` declares. That is the whole reason they have five decimal places.
 *
 * The *directions* are the design, and they are what carries to another theme:
 * the fills and rules step from the **panel** toward the text (a raised row is
 * panel lifted slightly), and the muted text steps from the **text** toward the
 * ground (dim is text let go of). On a light theme both reverse by construction,
 * which is the point — a light board gets darker borders rather than invisible
 * ones. */
const FROM_PANEL = {
  '--hover': 0.03209,
  '--raised': 0.05348,
  '--line-soft': 0.08021,
  '--line': 0.11230,
};
const FROM_TEXT = {
  '--dim': 0.21649,
  '--faint-solid': 0.35052,
  '--ghost': 0.46392,
};

/** Every derived token, from the three a person chose.
 *
 *  Returns CSS custom property names so the caller can hand them straight to
 *  `setProperty` — and so this file never touches the DOM itself.
 *
 *  **`--ground` and `--panel-ground` are the two see-through fills, and `--bg` and
 *  `--surface` are their solid twins.** The split is deliberate: eighteen rules use
 *  `--bg` for input grounds and hover fills and twenty-four use `--surface` for
 *  buttons, overlays and menus, and a text field you can read the desktop through
 *  is not a feature. Only the six panes that tile the window take the alpha.
 *
 *  **`--base` is the body's fill, and it is `transparent` the moment anything is
 *  see-through.** That is not an optimisation — it is what stops the alpha
 *  compounding. A translucent rail over a translucent body is two layers, so a
 *  board at 70% reads as 91% under the rail and 70% two pixels away. Exactly one
 *  element may paint each pixel, so when the panes take over the painting the body
 *  has to stop. It keeps painting while the board is solid, where there is no alpha
 *  to stack and a canvas with no fill is a white flash.
 */
export function tokens(/** @type {Palette} */ { bg, panel, text }, /** @type {{ opacity?: number }} */ { opacity = 1 } = {}) {
  const see = opacity < 1;
  /** @type {Record<string, string>} */
  const out = {
    '--bg': bg,
    '--ground': see ? alpha(bg, opacity) : bg,
    '--panel-ground': see ? alpha(panel, opacity) : panel,
    '--base': see ? 'transparent' : bg,
    '--surface': panel,
    '--text': text,
  };
  for (const [name, t] of Object.entries(FROM_PANEL)) out[name] = mix(panel, text, t);
  for (const [name, t] of Object.entries(FROM_TEXT)) out[name] = mix(text, bg, t);
  return out;
}

/** `#rrggbb` plus an alpha, as `rgba()`. */
export function alpha(/** @type {string} */ hex, /** @type {number} */ a) {
  const rgb = parseHex(hex);
  if (!rgb) return hex;
  return `rgba(${rgb.join(',')},${a})`;
}

/** A signal colour, kept as itself where it can be read and lifted where it cannot.
 *
 *  **The hue is the vocabulary and must not move**: amber means needs-you, green
 *  means running, and a theme that could turn amber grey would be turning a signal
 *  off rather than restyling it. So these stay out of `tokens()`.
 *
 *  But keeping their *luminance* fixed is what fails. Measured against a light
 *  ground of `#F4F2ED`: `--attn` lands at 1.99:1 and `--focus` at 1.48:1, so on a
 *  light board the needs-you signal is nearly invisible and the keyboard focus ring
 *  is invisible outright. That is the defect this exists for.
 *
 *  So the colour is mixed toward the **text** colour until it clears the floor —
 *  toward light on a dark board, toward dark on a light one, by construction. The
 *  hue survives; only how far it is from the ground changes. On the shipped dark
 *  palette every semantic already clears 4.5 (the lowest is `--auto` at 5.63), so
 *  this returns them untouched and the default board does not move.
 */
export function readable(/** @type {string} */ colour, /** @type {Palette} */ { bg, text }, floor = MIN_CONTRAST) {
  if (contrast(colour, bg) >= floor) return colour;
  // Coarse enough to be one pass and fine enough that the step is not visible.
  for (let t = 0.04; t <= 1; t += 0.04) {
    const lifted = mix(colour, text, t);
    if (contrast(lifted, bg) >= floor) return lifted;
  }
  // The text colour itself is the far end, and the pair that got here already
  // failed `legible` — so this is unreachable from a theme the app accepted.
  return text;
}

/** Whether a chosen pair can be read at all.
 *
 *  The pane refuses below this rather than correcting it: a board is the colours
 *  you picked, and quietly moving them is a worse answer than saying no. The
 *  refusal is also what keeps the way back reachable — a theme that made the
 *  settings pane invisible could only be undone by clearing `localStorage`.
 */
export function legible(/** @type {Palette} */ { bg, text }) {
  return contrast(bg, text) >= MIN_CONTRAST;
}

/** xterm's palette, from the same three colours.
 *
 *  **Derived rather than written out.** This was a literal table repeating `--bg`
 *  and `--text` in a second place, which is how a pane and the board come to
 *  disagree — and it meant the terminal ignored a theme entirely.
 *
 *  The sixteen ANSI colours keep their hues and are made readable against the
 *  ground the same way the signals are. The branch this replaces tinted the normal
 *  eight *toward the ground* by 12% and left the bright eight as literals, which on
 *  a light theme moved them the wrong way — `brightYellow` landed at 1.65:1 — and
 *  left half the palette hard-coded in the file that exists to stop exactly that.
 *
 *  On the shipped palette this returns **exactly today's terminal**: the three grey
 *  ratios are solved from it, and every one of the twelve hues already clears the
 *  floor against `#101010` (the worst is `red` at 4.84), so `readable` passes them
 *  through untouched.
 *
 *  **Always opaque**, whatever the window is doing: `allowTransparency` reaches
 *  xterm's DOM renderer and not its WebGL one, so a see-through terminal would
 *  force the canvas off — and on macOS the agent pane is the one place WebGL is
 *  still used, precisely because dropping it brings the typing lag back on the pane
 *  you type into. A see-through board with a solid terminal costs nothing; the
 *  reverse costs the typing.
 */
export function termColours(/** @type {Palette} */ t) {
  const on = (/** @type {string} */ c) => readable(c, t);
  return {
    background: t.bg,
    foreground: t.text,
    cursor: t.text,
    cursorAccent: t.bg,
    selectionBackground: mix(t.bg, t.text, 0.14433),
    black: t.bg,
    red: on('#C9615A'), green: on('#5FA97C'), yellow: on('#E0A244'),
    blue: on('#4C9AAF'), magenta: on('#9A7AA0'), cyan: on('#3E9AAF'),
    white: t.text,
    brightBlack: mix(t.bg, t.text, 0.38660),
    brightRed: on('#D6756E'), brightGreen: on('#74BB90'), brightYellow: on('#EDB55C'),
    brightBlue: on('#63AEC2'), brightMagenta: on('#B08FB6'), brightCyan: on('#57AEC2'),
    /* Past the text colour, *away* from the ground — lighter than text on a dark
       board and darker on a light one. Never toward literal white, which is what
       made "bright white" dimmer than normal text on a light theme. */
    brightWhite: mix(t.text, t.bg, -0.15464),
  };
}
