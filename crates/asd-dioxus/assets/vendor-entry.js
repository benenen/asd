// Vendor bundle entry — everything the webview needs from npm, exposed on
// `window`. esbuild bundles + minifies this into a single IIFE at build time
// (see package.json's `build` script, driven by build.rs).
//
// Adding a new npm dependency: `npm install <pkg>` and add an import +
// window assignment here — build.rs needs no change.
import * as GhosttyWeb from 'ghostty-web';

window.GhosttyWeb = GhosttyWeb;
