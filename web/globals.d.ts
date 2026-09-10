// What the vendored classic scripts put on the page before the modules run.
//
// They are loaded with plain <script> tags in index.html, so they are globals
// rather than imports — the type checker has no other way to know they exist.
// Deliberately loose: these are third-party surfaces we drive, not model.

declare const Terminal: any;
declare const FitAddon: any;
declare const WebglAddon: any;
declare const Prism: any;

interface Window {
  /**
   * Injected into the served page: the API token, how to draw the titlebar, and
   * which platform the daemon runs on — `"mac"` or `"other"`, which decides
   * whether the app's chords wear ⌘ or Ctrl. Told rather than sniffed, because
   * the daemon knows at compile time.
   */
  __ORCH__: {
    token: string;
    chrome: string;
    platform: string;
    /**
     * Every checkout this window is showing, **this one first**, each a whole
     * separate daemon on its own port (`src/peers.rs`).
     *
     * Not `repos`: the snapshot's own `repos` is the GitHub pair a checkout
     * pushes to, and two names one letter apart for two different things is how a
     * reader ends up drawing the wrong one.
     *
     * **Empty is the single-repository shape**, not a degraded one: no shell
     * attached a list, so `core.js` builds the one local entry itself. That is
     * every install that existed before this feature, so a reader must treat it
     * as normal.
     *
     * Hand-written rather than generated: `snapshot.d.ts` comes from the structs
     * a *snapshot* is built of, and this is deliberately not in the snapshot —
     * a daemon must stay unaware that it has siblings.
     */
    checkouts: { id: string; name: string; path: string; port: number; token: string; colour: string }[];
  };
  /** Prism is driven manually; this switches its auto-highlight off. */
  Prism: any;
  WebglAddon: any;
  /** Hung on the global object by the SPA, for the drawer. */
  orchTeardown: (wsId: string) => void;
  /** The macOS menu bar's Settings item, which is native and evals this name. */
  orchSettings: () => void;
}
