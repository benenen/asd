#!/usr/bin/env node
'use strict';

// postinstall: fetch the prebuilt `asd` binary matching this platform from the
// GitHub Release tagged v<version>, unpack it, and place it beside the launcher
// (lib/asd[.exe]) with the execute bit set.
//
// The asset contract is fixed by .github/workflows/release.yml:
//   name:   asd-<version>-<target>.<ext>   (ext = tar.gz on unix, zip on win)
//   layout: a top-level dir asd-<version>-<target>/ containing asd[.exe]
//
// Env overrides:
//   ASD_SKIP_DOWNLOAD=1     do nothing (offline, or CI of this package itself)
//   ASD_BINARY_PATH=<file>  copy an already-built binary in, no network
//   ASD_DOWNLOAD_BASE=<url> base URL for release assets (mirror / proxy);
//                           default https://github.com/benenen/asd/releases/download
//   ASD_VERSION=<x.y.z>     release version to fetch (default: package version)

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const { execFileSync } = require('child_process');

const { rustTarget, exeName, binaryPath } = require('./lib/shared');

const REPO = 'benenen/asd';
const pkg = require('./package.json');

function log(msg) {
  process.stderr.write(`[asd install] ${msg}\n`);
}
function fail(msg) {
  log('ERROR: ' + msg);
  process.exit(1);
}

/** Release asset filename for a version/platform/arch, or `null` if unsupported. */
function assetName(version, platform = process.platform, arch = process.arch) {
  const target = rustTarget(platform, arch);
  if (!target) return null;
  const ext = platform === 'win32' ? 'zip' : 'tar.gz';
  return `asd-${version}-${target}.${ext}`;
}

/** Full download URL for the matching asset, or `null` if unsupported. */
function downloadUrl(version, platform = process.platform, arch = process.arch, base) {
  const name = assetName(version, platform, arch);
  if (!name) return null;
  const root = (base || process.env.ASD_DOWNLOAD_BASE ||
    `https://github.com/${REPO}/releases/download`).replace(/\/+$/, '');
  return `${root}/v${version}/${name}`;
}

/** GET `url` to `dest`, following redirects (GitHub → object storage). */
function download(url, dest, cb, redirects) {
  redirects = redirects || 0;
  if (redirects > 10) return cb(new Error('too many redirects'));
  const req = https.get(url, { headers: { 'User-Agent': 'asd-npm-installer' } }, (res) => {
    const { statusCode, headers } = res;
    if (statusCode >= 300 && statusCode < 400 && headers.location) {
      res.resume();
      return download(new URL(headers.location, url).toString(), dest, cb, redirects + 1);
    }
    if (statusCode !== 200) {
      res.resume();
      return cb(new Error(`HTTP ${statusCode} for ${url}`));
    }
    const out = fs.createWriteStream(dest);
    res.pipe(out);
    out.on('finish', () => out.close(() => cb(null)));
    out.on('error', cb);
  });
  req.on('error', cb);
  req.setTimeout(60000, () => req.destroy(new Error('timeout after 60s')));
}

/** Unpack `archive` (a .tar.gz or .zip) into `dir` using system tools. */
function extract(archive, dir, ext) {
  if (ext === 'zip') {
    if (process.platform === 'win32') {
      // Expand-Archive ships with every supported Windows PowerShell.
      execFileSync('powershell', [
        '-NoProfile', '-NonInteractive', '-Command',
        `Expand-Archive -LiteralPath "${archive}" -DestinationPath "${dir}" -Force`
      ], { stdio: 'ignore' });
    } else {
      execFileSync('unzip', ['-o', archive, '-d', dir], { stdio: 'ignore' });
    }
  } else {
    // `tar` is present on linux, macOS, and Windows 10+ (bsdtar).
    execFileSync('tar', ['-xzf', archive, '-C', dir], { stdio: 'ignore' });
  }
}

/** Depth-first search for a file named `exe` beneath `dir`. */
function findBinary(dir, exe) {
  const stack = [dir];
  while (stack.length) {
    const d = stack.pop();
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) stack.push(p);
      else if (e.name === exe) return p;
    }
  }
  return null;
}

function place(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
  if (process.platform !== 'win32') fs.chmodSync(dest, 0o755);
}

function cleanup(dir) {
  try { fs.rmSync(dir, { recursive: true, force: true }); } catch (_) { /* best effort */ }
}

function main() {
  if (process.env.ASD_SKIP_DOWNLOAD === '1') {
    log('ASD_SKIP_DOWNLOAD=1 — leaving the binary in place / absent.');
    return;
  }

  const target = rustTarget();
  if (!target) {
    fail(
      `no prebuilt binary for ${process.platform}/${process.arch}. ` +
      `Available: linux & macOS (x64 + arm64) and windows x64. ` +
      `Build from source: https://github.com/${REPO}`
    );
  }

  const dest = binaryPath();
  const exe = exeName();

  // Dev / offline path: install a binary we already have.
  const local = process.env.ASD_BINARY_PATH;
  if (local) {
    if (!fs.existsSync(local)) fail(`ASD_BINARY_PATH does not exist: ${local}`);
    log(`ASD_BINARY_PATH set — installing ${local}`);
    place(local, dest);
    log(`installed → ${dest}`);
    return;
  }

  const version = process.env.ASD_VERSION || pkg.version;
  const ext = process.platform === 'win32' ? 'zip' : 'tar.gz';
  const url = downloadUrl(version);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'asd-dl-'));
  const archive = path.join(tmp, assetName(version));

  log(`downloading asd ${version} (${target})`);
  log(url);
  download(url, archive, (err) => {
    if (err) {
      cleanup(tmp);
      fail(
        `download failed: ${err.message}\n` +
        `  - check your network / proxy, or set ASD_DOWNLOAD_BASE to a mirror\n` +
        `  - confirm the release exists: https://github.com/${REPO}/releases/tag/v${version}`
      );
    }
    try {
      extract(archive, tmp, ext);
      const inner = path.join(tmp, `asd-${version}-${target}`, exe);
      const src = fs.existsSync(inner) ? inner : findBinary(tmp, exe);
      if (!src) throw new Error(`'${exe}' not found inside the archive`);
      place(src, dest);
      cleanup(tmp);
      log(`installed asd ${version} (${target}) → ${dest}`);
    } catch (e) {
      cleanup(tmp);
      fail(`unpack failed: ${e.message}`);
    }
  });
}

module.exports = { assetName, downloadUrl, main };

if (require.main === module) main();
