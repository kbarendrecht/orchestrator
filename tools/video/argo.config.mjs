import { defineConfig } from '@argo-video/cli';

// `run.mjs` sets the base: the host's page port is chosen at start, so it
// cannot be written here.
export default defineConfig({
  baseURL: process.env.ORCHD_VIDEO_BASE,
  demosDir: 'demos',
  outputDir: 'videos',
  video: {
    width: 1440,
    height: 900,
    fps: 30,
    browser: 'chromium',
    captureMode: 'jpeg-stitch',
    // Captured at twice the size and scaled down, which is what keeps text sharp
    // inside a camera zoom.
    deviceScaleFactor: 2,
    cursorHighlight: { mode: 'click', radius: 22, opacity: 0.9 },
  },
  export: { encoder: 'cpu', preset: 'slow', crf: 20 },
});
