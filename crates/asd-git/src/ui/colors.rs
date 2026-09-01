//! The lane palette.
//!
//! keifu uses eleven named ANSI colours. This overlay is drawn on top of asd's
//! own themed UI, whose palette is RGB throughout, and named colours would
//! follow the host terminal's theme and read as foreign. These eleven are in
//! the same family as `asd-tui`'s `ui.rs` constants and are a const so they can
//! be retuned without touching the layout code.

use ratatui::style::Color;

pub const LANE_COLORS: [Color; 11] = [
    Color::Rgb(0xF3, 0xB2, 0x4C), // accent (the trunk)
    Color::Rgb(0x79, 0xD1, 0x8C), // green
    Color::Rgb(0x6F, 0xB3, 0xE0), // blue
    Color::Rgb(0xD9, 0x8D, 0xD4), // magenta
    Color::Rgb(0xE5, 0x89, 0x5E), // orange
    Color::Rgb(0x5F, 0xC9, 0xA8), // aqua
    Color::Rgb(0x8B, 0x94, 0xA2), // muted
    Color::Rgb(0xC4, 0xD1, 0x6B), // olive
    Color::Rgb(0xE5, 0x59, 0x5E), // alert red
    Color::Rgb(0x9A, 0x8C, 0xE0), // violet
    Color::Rgb(0x5A, 0xC8, 0xC8), // teal
];

/// The colour for a lane index, rotating through the palette. Lane indices are
/// unbounded and the palette is not, so this must never index directly.
pub fn lane_color(index: usize) -> Color {
    LANE_COLORS[index % LANE_COLORS.len()]
}

/// Stable foreground/background colours for one branch-name label.
///
/// The hash selects one of 1,536 evenly stepped points around a saturated RGB
/// colour wheel, rather than one of the eleven lane colours: refs are names,
/// not lanes, and two nearby labels need more room before their colours repeat.
/// Saturation and value stay bounded so arbitrary branch text cannot generate
/// a muddy grey or a background too bright for either foreground choice.
pub fn branch_label_colors(name: &str) -> (Color, Color) {
    // FNV-1a is written out so the mapping is stable across processes and Rust
    // versions; DefaultHasher deliberately does not promise either property.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    const LOW: u16 = 48;
    const HIGH: u16 = 176;
    const STEPS: u16 = 256;
    let hue = (hash % u64::from(6 * STEPS)) as u16;
    let sector = hue / STEPS;
    let offset = hue % STEPS;
    let rising = LOW + (HIGH - LOW) * offset / (STEPS - 1);
    let falling = HIGH + LOW - rising;
    let (r, g, b) = match sector {
        0 => (HIGH, rising, LOW),
        1 => (falling, HIGH, LOW),
        2 => (LOW, HIGH, rising),
        3 => (LOW, falling, HIGH),
        4 => (rising, LOW, HIGH),
        _ => (HIGH, LOW, falling),
    };
    let (r, g, b) = (r as u8, g as u8, b as u8);
    let background_luminance = relative_luminance(r, g, b);
    // Pure RGB endpoints guarantee that at least one candidate reaches 4.5:1
    // against every possible background luminance (the worst crossover is
    // still sqrt(21), about 4.58:1).
    let dark = (0x00, 0x00, 0x00);
    let light = (0xFF, 0xFF, 0xFF);
    let dark_contrast = contrast(
        background_luminance,
        relative_luminance(dark.0, dark.1, dark.2),
    );
    let light_contrast = contrast(
        background_luminance,
        relative_luminance(light.0, light.1, light.2),
    );
    let foreground = if dark_contrast >= light_contrast {
        Color::Rgb(dark.0, dark.1, dark.2)
    } else {
        Color::Rgb(light.0, light.1, light.2)
    };
    (foreground, Color::Rgb(r, g, b))
}

/// WCAG relative luminance for one sRGB colour.
fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let linear = |channel: u8| {
        let channel = f64::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn contrast(a: f64, b: f64) -> f64 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("expected RGB, got {color:?}");
        };
        let linear = |channel: u8| {
            let channel = f64::from(channel) / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    fn contrast_ratio(a: Color, b: Color) -> f64 {
        let a = relative_luminance(a);
        let b = relative_luminance(b);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn the_palette_rotates_and_never_panics() {
        // Any index is valid: lane indices are unbounded, the palette is not.
        assert_eq!(lane_color(0), LANE_COLORS[0]);
        assert_eq!(lane_color(LANE_COLORS.len()), LANE_COLORS[0]);
        assert_eq!(lane_color(LANE_COLORS.len() + 3), LANE_COLORS[3]);
        assert_eq!(
            lane_color(usize::MAX),
            LANE_COLORS[usize::MAX % LANE_COLORS.len()]
        );
    }

    #[test]
    fn every_lane_colour_is_rgb() {
        // Named ANSI colours would follow the host terminal's theme and clash
        // with asd's own palette, which is RGB throughout.
        for (i, c) in LANE_COLORS.iter().enumerate() {
            assert!(
                matches!(c, ratatui::style::Color::Rgb(..)),
                "LANE_COLORS[{i}] is {c:?}, expected an Rgb value"
            );
        }
    }

    #[test]
    fn lane_colours_are_distinct() {
        for (i, a) in LANE_COLORS.iter().enumerate() {
            for (j, b) in LANE_COLORS.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "LANE_COLORS[{i}] and [{j}] are the same colour");
                }
            }
        }
    }

    #[test]
    fn branch_colours_are_stable_distinct_and_rgb() {
        let main = branch_label_colors("main");
        assert_eq!(main, branch_label_colors("main"));
        assert_ne!(main.1, branch_label_colors("origin/main").1);
        assert!(matches!(main.0, Color::Rgb(..)));
        assert!(matches!(main.1, Color::Rgb(..)));
    }

    #[test]
    fn branch_text_keeps_accessible_contrast_on_a_bright_green() {
        // This name lands on a green that the old integer-luminance cutoff
        // paired with white, producing only about 2.6:1 contrast.
        let (foreground, background) = branch_label_colors("feature/160");
        assert!(
            contrast_ratio(foreground, background) >= 4.5,
            "foreground {foreground:?} is too close to {background:?}"
        );
    }

    #[test]
    fn branch_text_keeps_accessible_contrast_on_a_mid_blue() {
        // This name reaches the worst part of the generated colour wheel for
        // the old near-black/near-white foreground pair (about 4.13:1).
        let (foreground, background) = branch_label_colors("feature/966");
        assert!(
            contrast_ratio(foreground, background) >= 4.5,
            "foreground {foreground:?} is too close to {background:?}"
        );
    }
}
