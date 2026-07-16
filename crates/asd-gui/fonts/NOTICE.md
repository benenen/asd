# Bundled fonts

## Noto Sans Symbols 2

`NotoSansSymbols2-Regular.ttf` is bundled and loaded at startup as a **symbol
fallback** for the terminal grid (see `src/lib.rs::run`). The primary monospace
face lacks media/technical glyphs (e.g. U+23F5 ⏵), and many systems — notably
the Windows client — ship no font that covers them, so those cells would
otherwise render as tofu boxes.

- Source: https://github.com/notofonts/symbols (Google Noto project)
- Copyright: © The Noto Project Authors
- License: SIL Open Font License, Version 1.1 — https://openfontlicense.org

The OFL permits bundling and redistribution with software. The font is used
here under its reserved family name, unmodified.
