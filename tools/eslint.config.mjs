// What `tsc` cannot answer about the SPA.
//
// `tsc --checkJs` answers "does this name exist, and what is its type". It has
// nothing to say about a promise nobody awaited — which is how
// `if (Diff.edit.on && !Diff.closeEditor())` shipped: `closeEditor` became
// `async` in the native-dialog migration, `!promise` is always `false`, and the
// split-mode toggle went ahead while the confirm box was still on screen.
// `no-misused-promises` reports that line and nothing else.
//
// Typed rules, so this needs the same `tsconfig.json` the type check uses —
// one definition of "the SPA", and a file added to `web/js/` is linted the
// moment it is served.

import tseslint from 'typescript-eslint';

export default tseslint.config(
  // `web/vendor/` is other people's code, shipped as it was downloaded.
  { ignores: ['web/vendor/**'] },
  {
    files: ['web/**/*.js'],
    extends: [tseslint.configs.recommendedTypeChecked],
    languageOptions: {
      parserOptions: {
        project: './tsconfig.json',
        tsconfigRootDir: import.meta.dirname,
      },
    },
    linterOptions: {
      // A rule this file turns off is turned off here, not at the site.
      reportUnusedDisableDirectives: true,
    },
    rules: {
      // The `any` that is left is deliberate and argued at each site: `ctl`,
      // xterm's own objects, and the two API payloads with no struct behind them.
      // These rules would report every read through one of those, which is noise
      // about a decision already made in writing.
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
      '@typescript-eslint/no-explicit-any': 'off',
      // `ctl(id)` is the one deliberate `any`, and it is argued in CLAUDE.md.
      '@typescript-eslint/no-redundant-type-constituents': 'off',
      // Hand-written JavaScript: `require` and `namespace` cannot appear.
      '@typescript-eslint/no-require-imports': 'off',
      // An import that is never used is a name somebody assumed was wired:
      // `term.js` imported `onThemeChange` and `toast` and called neither.
      // `catch (e) {}` and an unused parameter are not that — half the reads in
      // `core.js` are deliberately allowed to fail — so neither is reported.
      '@typescript-eslint/no-unused-vars': [
        'error',
        { args: 'none', caughtErrors: 'none', varsIgnorePattern: '^_' },
      ],

      // The two that pay for this file.
      //
      // A promise in a boolean position, or an `async` function handed to
      // something that wants `void`. The first is the bug above; the second is
      // an `onclick` whose rejection nobody catches, which is why
      // `checksVoidReturn` stays on for attributes and off for plain properties
      // — `el.onclick = async () => …` is how every handler here is written.
      '@typescript-eslint/no-misused-promises': [
        'error',
        { checksVoidReturn: { attributes: true, properties: false, variables: false } },
      ],
      // Its reach is the direct test of a conditional, measured rather than
      // assumed: `a && !p()` is reported, a bare `if (!p())` is not. So it
      // catches the guard that reads as a guard, which is the shape that shipped.
      //
      // `ignoreVoid` is the escape hatch: `void f()` says "fire and forget, on
      // purpose" once, and the rule then catches the calls that were not.
      '@typescript-eslint/no-floating-promises': ['error', { ignoreVoid: true }],

      // CLAUDE.md: "Never reach for the native three again." WKWebView shows a
      // script dialog only if the host implements the matching `WKUIDelegate`
      // method, and wry implements none of them — so `confirm()` returns false,
      // `prompt()` returns null and `alert()` does nothing, on macOS only.
      // `core.confirmBox` / `core.promptBox` are the replacements.
      'no-restricted-globals': [
        'error',
        { name: 'confirm', message: 'no-op in WKWebView — use core.confirmBox (async).' },
        { name: 'prompt', message: 'no-op in WKWebView — use core.promptBox (async).' },
        { name: 'alert', message: 'no-op in WKWebView — use core.toast.' },
      ],
      'no-restricted-properties': [
        'error',
        { object: 'window', property: 'confirm', message: 'no-op in WKWebView — use core.confirmBox (async).' },
        { object: 'window', property: 'prompt', message: 'no-op in WKWebView — use core.promptBox (async).' },
        { object: 'window', property: 'alert', message: 'no-op in WKWebView — use core.toast.' },
      ],

      // **The host's routes must go to the host.** `call` aims at the active
      // checkout's daemon, which answers `200 {}` to a route it does not have —
      // so every titlebar button once shipped silently dead under the app:
      // minimise, close, drag, resize and restart all succeeded at nothing.
      // `src/host.rs` owns `/api/window/*` and `/api/host/*`; `callHost` is the
      // seam. Both spellings, because these are built as template literals as
      // often as they are written out.
      'no-restricted-syntax': [
        'error',
        {
          selector: "CallExpression[callee.name='call'] > Literal[value=/^\\/api\\/(window|host)\\//]",
          message: 'a host route must go through callHost — `call` reaches the checkout daemon, which answers {} and looks like nothing happened.',
        },
        {
          selector: "CallExpression[callee.name='call'] > TemplateLiteral > TemplateElement[value.raw=/^\\/api\\/(window|host)\\//]",
          message: 'a host route must go through callHost — `call` reaches the checkout daemon, which answers {} and looks like nothing happened.',
        },
      ],

      eqeqeq: ['error', 'always', { null: 'ignore' }],
      'no-var': 'error',
      'no-implicit-globals': 'error',
    },
  },
);
