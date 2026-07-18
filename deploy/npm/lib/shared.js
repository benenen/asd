'use strict';

// Shared platform detection for the installer and the launcher. Kept
// dependency-free and side-effect-free so `node --check` and the smoke test can
// exercise it directly.

const path = require('path');

// npm platform (process.platform) + arch (process.arch) → Rust target triple,
// matching exactly the release assets built by .github/workflows/release.yml.
// Anything absent here (e.g. windows-arm64, linux-musl) has no prebuilt asset.
const TARGETS = {
  'linux x64': 'x86_64-unknown-linux-gnu',
  'linux arm64': 'aarch64-unknown-linux-gnu',
  'darwin x64': 'x86_64-apple-darwin',
  'darwin arm64': 'aarch64-apple-darwin',
  'win32 x64': 'x86_64-pc-windows-msvc'
};

/** The Rust target triple for a platform/arch, or `null` if none is published. */
function rustTarget(platform = process.platform, arch = process.arch) {
  return TARGETS[`${platform} ${arch}`] || null;
}

/** The binary's filename: `asd.exe` on Windows, `asd` everywhere else. */
function exeName(platform = process.platform) {
  return platform === 'win32' ? 'asd.exe' : 'asd';
}

/** Absolute path where the installer drops (and the launcher runs) the binary. */
function binaryPath(platform = process.platform) {
  // Lives next to this module, i.e. <pkg>/lib/asd[.exe].
  return path.join(__dirname, exeName(platform));
}

module.exports = { TARGETS, rustTarget, exeName, binaryPath };
