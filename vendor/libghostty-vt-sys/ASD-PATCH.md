# Local portability patch

Source: crates.io `libghostty-vt-sys` 0.2.1, upstream
`uzaaft/libghostty-rs` commit `46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0`.

The only upstream source change is `-Dcpu=baseline` on every `zig build`
invocation in `build.rs`. Native builds otherwise inherit the build host CPU
and can emit instructions unavailable on other machines (v0.2.0 failed with
SIGILL in `ghostty_formatter_terminal_new` on an Intel Xeon Gold 5318Y).
Cross-target selection, optimization mode, bindings and upstream version remain
unchanged. Remove this patch when an upstream release enforces a portable CPU
target. Test the emitted native/cross arguments with
`python3 scripts/test_ghostty_cpu_baseline.py`, and validate packaged binaries
with `python3 scripts/release_smoke.py /path/to/asd`.
