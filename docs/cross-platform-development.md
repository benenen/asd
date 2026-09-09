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
    --no-default-features
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

When a change touches the desktop notification adapter, separately compile the
real GUI target:

```bash
cargo check -p asd-dioxus --target x86_64-pc-windows-gnu
```

This checks that the Windows adapter remains behind `asd-dioxus/src/platform/`.
It needs a usable MinGW C toolchain for native dependencies. Zig 0.15.x can
also provide the C compiler and archiver: a local compiler wrapper must map
cc-rs's `--target=x86_64-pc-windows-gnu` to Zig's
`--target=x86_64-windows-gnu`. Set the target-specific `CC` and `AR` environment
variables to those wrappers. With aws-lc-sys 0.42, systems without NASM can use
`AWS_LC_SYS_PREBUILT_NASM=1` to select the dependency's bundled assembly objects.
Keep these overrides local to the check command. If neither local toolchain
works, require the native Windows full-GUI CI result. A successful cross-compile
still does not prove that Windows displayed a notification; keep the adapter's behavioral
tests and a real-machine smoke in the evidence.

## Session identity, persistence, and foreground proof

Every spawned child receives `ASD_SESSION_ID`, an opaque daemon-issued identity
that is stable across rename and fresh on replacement or restore. It is used by
the Codex/Claude hook command to locate its hosting session, not as evidence
that the current foreground process is Codex or Claude. A Start hook requires
foreground executable and argument proof. Where foreground lookup is
unavailable, the daemon may use only a conservative recorded launch command;
otherwise it rejects the report. Consequently, a coding agent launched manually
from a plain Windows shell may not acquire resume metadata until Windows
foreground lookup can prove it.

The versioned session store uses platform-specific private temporary-file
creation, replacement, and parent-directory synchronization. Preserve that
interface in `asd-daemon/src/platform/`; do not emulate Unix rename behavior at
call sites. Windows named pipes and session-store paths have different shapes,
so derive the PID-file and persistence locations from the data-directory
contract rather than by adding a file extension to a pipe name.

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

## Portable terminal-engine builds

The repository patches `libghostty-vt-sys` locally to pass `-Dcpu=baseline` to
Zig for native and cross builds. Without this override, native builds can emit
instructions available only on the build runner's CPU, even when Rust uses a
portable target. Keep `ReleaseFast` and the existing target/ABI handling; Rust
`target-cpu` flags alone do not constrain the Zig library. The patch rationale
and source provenance live in `vendor/libghostty-vt-sys/ASD-PATCH.md`. Release
preflights and tag builds run `scripts/test_ghostty_cpu_baseline.py` and the
workflow lifecycle contracts before draft creation. This argument-level gate
complements the downloaded-binary smoke test on another CPU; it does not replace
runtime validation.

## Release lifecycle: build, verify, then publish

A version tag now creates a **draft** GitHub Release. Linux x64/arm64, macOS
arm64 and Windows x64 archives are attached while it remains a draft. Release
notes are assembled without publishing it; no npm package is published on the
tag push. A manual `Release` workflow dispatch is build-only, including when
its selected ref is a tag.

Before tagging, finish the checks above and the four-platform build preflight.
Use an annotated tag with a version subject and Markdown notes, preserving
headings with `git tag -a vX.Y.Z --cleanup=verbatim -F notes.md`. Never move a
published version tag to repair an artifact: use a new patch version.

After the tag workflow (including release notes) has finished successfully:

1. Download the actual draft release archives with an authenticated `gh release
   download vX.Y.Z`. Confirm all four archives, their layout, version output,
   and the Windows `ghostty-vt.dll` sidecar.
2. Run the downloaded binary through an isolated daemon, a real PTY session,
   TUI attach/Snapshot, output, and resize. Exercise the new release's features.
   Include another machine/CPU for native-library portability; `--version`
   alone does not exercise the terminal engine.
3. Only after validation, publish the draft using an authorized personal access
   token or GitHub App token:

   ```bash
   gh release edit vX.Y.Z --draft=false --latest
   ```

4. The `release.published` event runs only the npm job. It checks out the release
   tag, verifies the package version and four nonempty release assets, then
   publishes `@shibenenen/asd` with `NPM_TOKEN`. Verify the npm version and a
   clean installation before reporting the release complete.

Do not publish with a workflow's `GITHUB_TOKEN`: GitHub normally suppresses
follow-up workflow events produced by that token. Use the authenticated operator
or App action above so npm publication is triggered. See GitHub's
[workflow trigger rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)
and [release events](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#release).

Stable published releases alone enter the npm job; publishing a prerelease does
not update npm. Draft creation and every individual upload recheck the release state, including
when only failed platform jobs are rerun. Uploads use `gh release upload` and
never create a release or change its draft state. This check and the upload are
separate API operations, not an atomic publication lock. Retry failures while the
release remains a draft, and never promote it while any build/upload/notes job
is still running.

For the 0.2.1 recovery only, after npm publication succeeds, the workflow marks
`@shibenenen/asd@0.2.0` deprecated with the known Linux SIGILL warning. The step
checks that 0.2.1 exists and skips an already-deprecated 0.2.0. Later versions do
not run this recovery step.

The offline workflow contracts can be checked with
`python3 scripts/test_release_workflow.py` (PyYAML required). These tests cover
event separation, draft-only asset uploads, refusal to rebuild public releases,
notes that preserve publication state, and the ordering/scope of deprecation.
