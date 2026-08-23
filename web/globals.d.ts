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
  __ORCH__: { token: string; chrome: string; platform: string };
  /** Prism is driven manually; this switches its auto-highlight off. */
  Prism: any;
  WebglAddon: any;
  /** The one thing the SPA still hangs on the global object, for the drawer. */
  orchTeardown: (wsId: string) => void;
}
