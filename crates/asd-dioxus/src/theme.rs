//! Terminal colors shared by the wire appearance, CSS, and ghostty-web.

use asd_proto::{TerminalAppearance, TerminalColor};

const FOREGROUND: TerminalColor = TerminalColor {
    r: 0xe7,
    g: 0xe2,
    b: 0xd6,
};
const BACKGROUND: TerminalColor = TerminalColor {
    r: 0x0b,
    g: 0x0d,
    b: 0x11,
};

pub(crate) const TERMINAL_APPEARANCE: TerminalAppearance = TerminalAppearance {
    foreground: Some(FOREGROUND),
    background: Some(BACKGROUND),
};

/// CSS custom properties consumed by both the app stylesheet and bridge JS.
pub(crate) fn css_custom_properties() -> String {
    format!(
        ":root{{--asd-terminal-foreground:#{:02X}{:02X}{:02X};--asd-terminal-background:#{:02X}{:02X}{:02X};}}",
        FOREGROUND.r, FOREGROUND.g, FOREGROUND.b, BACKGROUND.r, BACKGROUND.g, BACKGROUND.b
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_wire_colors_as_css_custom_properties() {
        assert_eq!(
            css_custom_properties(),
            ":root{--asd-terminal-foreground:#E7E2D6;--asd-terminal-background:#0B0D11;}"
        );
    }
}
