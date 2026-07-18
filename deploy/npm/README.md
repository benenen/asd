# asd (npm installer)

Install [`asd`](https://github.com/benenen/asd) — a GPU terminal client + headless
mux daemon (shpool-style: one session = one PTY) — via npm. This package ships no
binary itself; on install it downloads the prebuilt executable that matches your
platform from the project's GitHub Releases.

## Install

```bash
# one-off, no install:
npx asd

# or globally (exposes the `asd` command):
npm install -g asd
asd            # launches the GUI
asd new work   # create a session
asd ui         # the terminal session switcher (TUI)
```

## Supported platforms

| OS            | x64 | arm64 |
|---------------|-----|-------|
| Linux (gnu)   | ✅  | ✅    |
| macOS         | ✅  | ✅    |
| Windows (msvc)| ✅  | —     |

These mirror the assets built by the release CI. On Windows the binary is the
GUI client (the daemon/CLI side is Unix-only). Any other platform/arch has no
prebuilt asset — the installer fails with a clear message and you can build from
source instead.

## How it works

`postinstall` runs [`install.js`](install.js), which:

1. Maps `process.platform` + `process.arch` to the Rust target triple that the
   release CI publishes (see [`lib/shared.js`](lib/shared.js)):
   - `linux/x64` → `x86_64-unknown-linux-gnu`
   - `linux/arm64` → `aarch64-unknown-linux-gnu`
   - `darwin/x64` → `x86_64-apple-darwin`
   - `darwin/arm64` → `aarch64-apple-darwin`
   - `win32/x64` → `x86_64-pc-windows-msvc`
2. Downloads the matching asset
   `asd-<version>-<target>.<tar.gz|zip>` from
   `https://github.com/benenen/asd/releases/download/v<version>/…`
   (the tag is `v` + this package's `version`, following redirects).
3. Unpacks it (`tar` on Unix, `Expand-Archive` on Windows), copies the binary to
   `lib/asd[.exe]` and sets the execute bit.

The `asd` command is a tiny Node launcher ([`bin/asd.js`](bin/asd.js)) that execs
that binary, forwarding argv, the tty (asd is a TUI), and the exit code.

## Environment overrides

| Variable | Effect |
|---|---|
| `ASD_DOWNLOAD_BASE` | Base URL for the release assets (mirror / behind a proxy). Default `https://github.com/benenen/asd/releases/download`. |
| `ASD_VERSION` | Release version to fetch (default: this package's `version`). |
| `ASD_BINARY_PATH` | Install a locally built binary instead of downloading (offline / dev). |
| `ASD_SKIP_DOWNLOAD=1` | Skip the download entirely. |

Behind a corporate proxy, point `ASD_DOWNLOAD_BASE` at an internal mirror of the
release assets.

## Releasing

The package `version` must equal the GitHub Release tag without the `v`
(tag `v1.2.3` ⇄ `"version": "1.2.3"`), so it stays in lockstep with the crate
version. To publish:

```bash
cd deploy/npm
npm version 1.2.3        # match the release tag
npm publish              # add --access public if you scope the name
```

If the bare name `asd` is taken on the registry, scope it (e.g. `@benenen/asd`);
the exposed command stays `asd` regardless of the package name.

## Validate without publishing

```bash
cd deploy/npm
npm test                 # syntax check + dry-run asset/URL mapping (no network)
```
