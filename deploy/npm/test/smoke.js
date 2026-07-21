'use strict';

// Dry-run validation — no network. Asserts the platform→asset mapping and the
// URL construction against the release.yml contract, and that the postinstall
// script is a no-op under ASD_SKIP_DOWNLOAD. Run with `npm test`.

const assert = require('assert');
const path = require('path');
const { spawnSync } = require('child_process');

const shared = require('../lib/shared');
const install = require('../install.js');

let checks = 0;
function ok(cond, label) {
  assert.ok(cond, label);
  checks++;
}
function eq(a, b, label) {
  assert.strictEqual(a, b, `${label}: ${JSON.stringify(a)} !== ${JSON.stringify(b)}`);
  checks++;
}

// 1) Every published target maps as release.yml names it.
eq(shared.rustTarget('linux', 'x64'), 'x86_64-unknown-linux-gnu', 'linux x64');
eq(shared.rustTarget('linux', 'arm64'), 'aarch64-unknown-linux-gnu', 'linux arm64');
eq(shared.rustTarget('darwin', 'arm64'), 'aarch64-apple-darwin', 'darwin arm64');
eq(shared.rustTarget('win32', 'x64'), 'x86_64-pc-windows-msvc', 'win32 x64');

// 2) Unsupported combos have no asset (must fail loudly, not guess).
eq(shared.rustTarget('darwin', 'x64'), null, 'darwin x64 unsupported (arm64-only macOS)');
eq(shared.rustTarget('win32', 'arm64'), null, 'win32 arm64 unsupported');
eq(shared.rustTarget('linux', 'ia32'), null, 'linux ia32 unsupported');
eq(shared.rustTarget('sunos', 'x64'), null, 'sunos unsupported');

// 3) Exe name + asset name + ext per platform.
eq(shared.exeName('win32'), 'asd.exe', 'win exe name');
eq(shared.exeName('linux'), 'asd', 'unix exe name');
eq(install.assetName('0.1.0', 'linux', 'x64'),
  'asd-0.1.0-x86_64-unknown-linux-gnu.tar.gz', 'linux asset');
eq(install.assetName('0.1.0', 'darwin', 'arm64'),
  'asd-0.1.0-aarch64-apple-darwin.tar.gz', 'macos asset');
eq(install.assetName('1.2.3', 'win32', 'x64'),
  'asd-1.2.3-x86_64-pc-windows-msvc.zip', 'windows asset (.zip)');
eq(install.assetName('0.1.0', 'win32', 'arm64'), null, 'unsupported asset is null');

// 4) URL: default host + tag = v<version>, and an override base wins.
eq(install.downloadUrl('0.1.0', 'linux', 'x64'),
  'https://github.com/benenen/asd/releases/download/v0.1.0/asd-0.1.0-x86_64-unknown-linux-gnu.tar.gz',
  'default url');
eq(install.downloadUrl('0.1.0', 'linux', 'x64', 'https://mirror.example/asd/'),
  'https://mirror.example/asd/v0.1.0/asd-0.1.0-x86_64-unknown-linux-gnu.tar.gz',
  'override base (trailing slash trimmed)');

// 5) postinstall is a clean no-op when told to skip the download.
const skip = spawnSync(process.execPath, ['install.js'], {
  cwd: path.join(__dirname, '..'),
  env: Object.assign({}, process.env, { ASD_SKIP_DOWNLOAD: '1' }),
  encoding: 'utf8'
});
eq(skip.status, 0, 'skip-download install exits 0');
ok(/skipping|leaving/i.test(skip.stderr), 'skip-download logs a skip notice');

console.log(`ok — ${checks} checks passed`);
