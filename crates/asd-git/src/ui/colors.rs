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

#[cfg(test)]
mod tests {
    use super::*;

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
}
