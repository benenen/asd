#!/usr/bin/env node
'use strict';

// Launcher shim: exec the prebuilt native `asd` (downloaded by install.js into
// lib/asd[.exe]), forwarding argv, stdio — asd is a TUI, so inherit the tty —
// and the child's exit status. A JS shim (rather than pointing `bin` straight at
// the native file) lets npm generate correct wrappers on every OS, including the
// Windows .cmd / .ps1 shims.

const fs = require('fs');
const { spawnSync } = require('child_process');
const { binaryPath } = require('../lib/shared');

const bin = binaryPath();

if (!fs.existsSync(bin)) {
  process.stderr.write(
    `asd: native binary missing (${bin}).\n` +
    `The postinstall download likely failed. Reinstall it with one of:\n` +
    `  npm rebuild asd\n` +
    `  npm install -g asd\n`
  );
  process.exit(1);
}

const res = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });

if (res.error) {
  process.stderr.write(`asd: failed to launch ${bin}: ${res.error.message}\n`);
  process.exit(1);
}
// Mirror the child: exit code, or 1 if it was terminated by a signal.
process.exit(res.status === null ? 1 : res.status);
