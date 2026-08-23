/* The SPA's module graph, as a rule set.
 *
 * The graph was made acyclic on purpose: `app.js` sits over the six feature
 * modules, `rail` over `term` and `review`, `review` over `diff`, and everything
 * over `core`. Three cycles had to be broken to get there (see CLAUDE.md), and
 * nothing but a check stops them coming back — ES modules allow cycles, so the
 * failure would be silent.
 */
module.exports = {
  forbidden: [
    {
      name: 'no-circular',
      comment:
        'A cycle here would work at runtime and quietly undo the inversions that ' +
        'made these modules a layering rather than six files that call each other.',
      severity: 'error',
      from: {},
      to: { circular: true },
    },
    {
      name: 'core-is-the-floor',
      comment:
        'core.js is the shared layer: the fetch wrappers, the snapshot, the ' +
        'selection, the vocabulary. It must not reach up into a feature — that is ' +
        'what `onScaleChange`, `onSelection` and `onDrawerChange` are for.',
      severity: 'error',
      from: { path: 'web/js/core\\.js$' },
      to: { path: 'web/(app\\.js|js/(term|rail|diff|review|queue|settings)\\.js)$' },
    },
    {
      name: 'features-do-not-import-the-entry',
      comment:
        'app.js owns boot order, the websocket and the keyboard map. A feature ' +
        'reaching back into it is the coupling these modules exist to end.',
      severity: 'error',
      from: { path: 'web/js/.+\\.js$' },
      to: { path: 'web/app\\.js$' },
    },
    {
      name: 'no-orphans',
      comment: 'A module nothing imports is either dead or unwired.',
      severity: 'error',
      from: { orphan: true, pathNot: '\\.d\\.ts$' },
      to: {},
    },
  ],
  options: {
    doNotFollow: { path: 'node_modules|web/vendor' },
    exclude: { path: 'web/vendor' },
    tsPreCompilationDeps: false,
    enhancedResolveOptions: { extensions: ['.js'] },
  },
};
