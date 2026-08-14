# Cross-platform development and verification

This document supplements the common checks in [`CLAUDE.md`](../CLAUDE.md).
Use it when a change touches platform adapters, terminal handling, paths,
processes, filesystem behavior, or release packaging.

## Build prerequisites

- Rust with edition 2024 support.
- Zig 0.15.x on `PATH` for the vendored libghostty-vt build.
- Node and npm for the `asd-dioxus` JavaScript bundle.
- WebKitGTK development/runtime libraries for full Linux GUI builds.

The normal repository-wide checks are:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

`cargo test` without `--workspace` covers only the root package. E2E tests under
`tests/` start real isolated daemons and sockets.

## Windows code from Linux

Use the GNU target, not MSVC:

```bash
rustup target add x86_64-pc-windows-gnu
env -i PATH="$PATH" HOME="$HOME" RUSTUP_HOME="$HOME/.rustup" CARGO_HOME="$HOME/.cargo" \
  LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast \
  cargo check --target x86_64-pc-windows-gnu \
    --no-default-features --features local
```

The clean `env -i` is required. A polluted shell environment or stdin can
corrupt Cargo's target probing and produce misleading errors such as
`unknown start of token: \\u{0}`.

Why this command has narrow choices:

- the MSVC target cannot build the vendored C++ locally because Zig does not
  carry the MSVC SDK headers required by highway/simdutf;
- Zig includes MinGW support, so the GNU target checks proto, client, VT,
  daemon, TUI, CLI, and the root binary together;
- do not add `--all-targets`: a dev-dependency build script then requires an
  external `x86_64-w64-mingw32-gcc` that this check does not provide.

This is a build-layer check, not proof of all Windows behavior.
[`windows-check.yml`](../.github/workflows/windows-check.yml) runs tests for
workspace members other than the root package and lints Windows-only library
paths plus the root binary. It deliberately excludes the Unix-only root E2E
target, so the Windows daemon has no native E2E coverage. A real Windows
terminal is still required for named pipes, console restoration, DLL
packaging, and URL detection.

## macOS checks

Foreground-command parsing on macOS uses `sysctl(KERN_PROCARGS2)` with a
libproc executable-path fallback. The pure `parse_procargs2` parser is compiled
under tests on Linux, while the FFI signature needs an Apple target check such
as:

```bash
cargo check --target x86_64-apple-darwin
```

Do not treat a Linux unit test as proof that macOS FFI or filesystem behavior
works. Conversely, keep the pure parser test platform-independent so the
format logic is continuously exercised.

## Case-insensitive filesystems

Windows and common macOS volumes cannot represent two directory entries that
differ only by case. Tests for `asd card` must not assume that `README.md` and
`readme.md` can coexist everywhere.

Test the name-selection rule in the pure `match_name` layer. At the filesystem
layer, enumerate the directory and adapt assertions to what that filesystem can
actually represent. Linux can approximate a case-folding volume for additional
coverage:

```bash
mkfs.ext4 -O casefold -F ci.img
mount -o loop ci.img mnt
mkdir mnt/tmp
chattr +F mnt/tmp
TMPDIR=$PWD/mnt/tmp cargo test -p asd-cli --lib card
```

The approximation has limits. Linux `canonicalize()` preserves the supplied
case, while NTFS canonicalization may return the stored spelling. A test that
compares canonicalized spellings can fail on the simulated volume and pass on
real Windows. Use CI evidence to distinguish an implementation defect from a
test premise that the target filesystem cannot express.

## Validation layers

Each layer proves something different:

| Layer | Proves | Does not prove |
|---|---|---|
| Linux workspace tests | Unit/integration/E2E behavior compiled for Linux | Windows/macOS-only `cfg` paths or host terminal behavior |
| Strict workspace Clippy | Warnings and lint contracts for host targets/tests | Foreign-target runtime behavior |
| Windows GNU cross-check | Windows production build paths compile through the local feature set | Windows test-only code or real console/named-pipe behavior |
| Windows CI | Native builds, non-root workspace tests, and Windows-only Clippy paths | Root/daemon E2E and interactive terminal behavior |
| Real-machine smoke | Packaging, startup, console, mouse, URL, and named-pipe behavior | Broad regression coverage |

Do not replace one layer with another in a handoff claim.

## Focused smoke checks

Useful local checks after terminal/session work:

```bash
cargo run -- attach -A demo
cargo run -- ui demo
cargo test --test e2e sigterm
```

Detach the CLI client with `Ctrl-\`. For TUI ownership changes, open two
independent host terminals, select the same session, confirm the first shows the
takeover placard, then select it again to reclaim. Keep an ordinary
`asd attach` connected throughout to verify shared clients are not revoked and
that PTY size recovers when either TUI exits.
