//! Draw a [`RenderSnapshot`] onto an iced canvas with a monospace font.
//! M0 scope (spec §7): monospace grid, ASCII correct; CJK/style fidelity is M1.

use asd_vt::{CellSnapshot, CellWidth, RenderSnapshot, Rgb, StyleFlags, UnderlineKind};
use iced::widget::canvas::{self, Cache, Frame, Geometry, Text};
use iced::widget::text::Shaping;

use iced::font::{Style, Weight};
use iced::{Color, Font, Point, Rectangle, Renderer, Size, Theme, mouse};

/// Fixed monospace cell metrics (approximate; text is not measured in M0).
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub font_size: f32,
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Metrics {
    pub fn new(font_size: f32) -> Self {
        Self {
            font_size,
            // JetBrains Mono advance ratio ~0.60 at typical sizes; line-height
            // ratio ~1.4 gives comfortable spacing (matching Ghostty's default).
            cell_w: (font_size * 0.60).max(1.0),
            cell_h: (font_size * 1.40).round().max(1.0),
        }
    }

    /// Primary monospace font (narrow cells). Uses the *generic* monospace
    /// family rather than a specific face name: `Font::with_name("…")` falls
    /// back to the platform's **proportional** default when the named font is
    /// absent (e.g. JetBrains Mono is not installed on stock macOS), whose
    /// per-glyph advances then overflow the fixed `col * cell_w` grid and make
    /// characters overlap ("@" bleeding into "f"). `Family::Monospace` always
    /// resolves to a real fixed-width face (Menlo on macOS, DejaVu/Liberation
    /// Mono on Linux), so glyph advances match the cell step. This is the same
    /// font the sidebar already uses.
    pub fn narrow_font() -> Font {
        Font::MONOSPACE
    }

    /// Font for wide (CJK) cells. `Family::Monospace` has no CJK glyphs, so
    /// cosmic-text falls back per-glyph to a CJK face; CJK glyphs are full
    /// width and we reserve two cells for them ([`cell_span`]).
    pub fn wide_font() -> Font {
        Font::MONOSPACE
    }

    /// The left pixel edge of grid column `col`. Cells are laid out on a strict
    /// fixed grid — position is `col * cell_w`, never the glyphs' natural
    /// advance — so text can't creep or overlap regardless of the font.
    pub fn cell_x(&self, col: u16) -> f32 {
        col as f32 * self.cell_w
    }

    /// The grid column containing pixel `px` (inverse of [`cell_x`]).
    pub fn col_at(&self, px: f32) -> u16 {
        (px / self.cell_w).max(0.0).floor() as u16
    }

    /// The grid row containing pixel `py`.
    pub fn row_at(&self, py: f32) -> u16 {
        (py / self.cell_h).max(0.0).floor() as u16
    }

    /// Terminal columns/rows that fit a window of `size` pixels.
    pub fn grid(&self, size: Size) -> (u16, u16) {
        let cols = (size.width / self.cell_w).floor().max(1.0) as u16;
        let rows = (size.height / self.cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }
}

/// How many grid columns a cell of the given width occupies: wide (CJK) cells
/// span two, everything else one. `SpacerTail` is the hidden second half of a
/// wide cell and is skipped by the caller, so it counts as one here.
pub fn cell_span(width: CellWidth) -> f32 {
    if width == CellWidth::Wide { 2.0 } else { 1.0 }
}

/// Project an absolute (scrollback-anchored) row to a viewport row, given the
/// viewport's absolute top row `base` (= scrollback_rows − scroll). Returns
/// `None` when the row is outside the visible `rows`. Because a fixed absolute
/// row maps through the *current* `base`, a selection anchored in absolute
/// coordinates stays on the same text as the viewport scrolls (`base` changes)
/// instead of pinning to a screen position.
pub fn viewport_row(abs_row: usize, base: usize, rows: u16) -> Option<usize> {
    let vpy = abs_row.checked_sub(base)?;
    (vpy < rows as usize).then_some(vpy)
}

/// The absolute (scrollback-anchored) row for viewport row `vpy` when the
/// viewport's top sits at absolute row `base`. Inverse of [`viewport_row`].
pub fn abs_row(base: usize, vpy: usize) -> usize {
    base + vpy
}

/// Canvas program that paints the current frame, highlights the active
/// selection, and emits messages for mouse wheel / drag events.
pub struct TermCanvas<'a> {
    pub frame: Option<&'a RenderSnapshot>,
    pub cache: &'a Cache,
    pub metrics: Metrics,
    pub selection: Option<crate::GuiSelection>,
    /// Absolute row of the viewport's top line (scrollback_rows − scroll), used
    /// to project the absolute-anchored selection back to viewport rows.
    pub base: usize,
}

impl<Message> canvas::Program<Message> for TermCanvas<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let sel = self.selection;
        let base = self.base;
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            let Some(snap) = self.frame else {
                frame.fill_rectangle(Point::ORIGIN, bounds.size(), Color::BLACK);
                return;
            };
            paint(frame, snap, self.metrics, bounds.size(), sel, base);
        });
        vec![geometry]
    }
}

fn paint(
    frame: &mut Frame,
    snap: &RenderSnapshot,
    m: Metrics,
    size: Size,
    sel: Option<crate::GuiSelection>,
    base: usize,
) {
    let default_bg = color(snap.background);
    let default_fg = color(snap.foreground);
    frame.fill_rectangle(Point::ORIGIN, size, default_bg);

    for (y, row) in snap.cells.iter().enumerate() {
        let py = y as f32 * m.cell_h;
        for (x, cell) in row.iter().enumerate() {
            if cell.width == CellWidth::SpacerTail {
                continue;
            }
            // Fixed grid: a cell's left edge is strictly `col * cell_w`, never
            // the glyph's natural advance (see [`Metrics::cell_x`]).
            let px = m.cell_x(x as u16);
            let span = cell_span(cell.width);

            // Selection highlight. The selection ends are anchored in absolute
            // scrollback rows; this cell's absolute row is `base + y`, so the
            // highlight follows the text as the viewport scrolls (base changes)
            // rather than pinning to a screen row.
            let selected = !cell.grapheme.is_empty()
                && sel.is_some_and(|s| {
                    let (a, b) = if (s.anchor.1, s.anchor.0) <= (s.head.1, s.head.0) {
                        (s.anchor, s.head)
                    } else {
                        (s.head, s.anchor)
                    };
                    let abs_y = abs_row(base, y);
                    abs_y >= a.1
                        && abs_y <= b.1
                        && (x as u16) >= if abs_y == a.1 { a.0 } else { 0 }
                        && (x as u16)
                            <= if abs_y == b.1 {
                                b.0
                            } else {
                                snap.cols.saturating_sub(1)
                            }
                });

            // Background: cell bg, or selection highlight.
            if selected {
                frame.fill_rectangle(
                    Point::new(px, py),
                    Size::new(m.cell_w * span, m.cell_h),
                    default_fg, // inverse: use fg as bg
                );
            } else if let Some(bg) = cell.bg {
                frame.fill_rectangle(
                    Point::new(px, py),
                    Size::new(m.cell_w * span, m.cell_h),
                    color(bg),
                );
            }

            if !cell.grapheme.is_empty() && cell.grapheme != " " {
                let fg = if selected {
                    default_bg // inverse: use bg as fg
                } else {
                    cell.fg.map(color).unwrap_or(default_fg)
                };
                let base = if cell.width == CellWidth::Wide {
                    Metrics::wide_font()
                } else {
                    Metrics::narrow_font()
                };
                let font = styled_font(base, &cell.flags);

                // Faint (dim) text: reduce opacity (skip when selected).
                let fg = if !selected && cell.flags.faint {
                    Color { a: 0.5, ..fg }
                } else {
                    fg
                };

                // Inverse-video swap (vim cursor line, etc.)
                let fg = if cell.flags.inverse {
                    cell.bg.map(color).unwrap_or(default_bg)
                } else {
                    fg
                };
                let bg = if cell.flags.inverse && cell.bg.is_none() {
                    Some(default_fg)
                } else {
                    cell.bg.map(color)
                };

                if let Some(bg) = bg {
                    frame.fill_rectangle(
                        Point::new(px, py),
                        Size::new(m.cell_w * span, m.cell_h),
                        bg,
                    );
                }

                // Skip invisible text (used for password fields etc.)
                if !cell.flags.invisible {
                    frame.fill_text(glyph(&cell.grapheme, px, py, fg, m, font));
                }

                // Underline / strikethrough / overline decoration.
                paint_decorations(frame, cell, px, py, m, fg);
            }
        }
    }

    paint_cursor(frame, snap, m, default_fg, default_bg);
}

/// Apply bold/italic weight to a base font.
fn styled_font(base: Font, flags: &StyleFlags) -> Font {
    Font {
        weight: if flags.bold {
            Weight::Bold
        } else {
            base.weight
        },
        style: if flags.italic {
            Style::Italic
        } else {
            base.style
        },
        ..base
    }
}

/// Draw underline / strikethrough / overline lines for a cell.
fn paint_decorations(
    frame: &mut Frame,
    cell: &CellSnapshot,
    px: f32,
    py: f32,
    m: Metrics,
    fg: Color,
) {
    let w = if cell.width == CellWidth::Wide {
        m.cell_w * 2.0
    } else {
        m.cell_w
    };

    // Underline.
    if cell.flags.underline != UnderlineKind::None {
        let y = py + m.cell_h - 2.0;
        let h = match cell.flags.underline {
            UnderlineKind::Double => 2.0,
            UnderlineKind::Curly => 1.0, // simplified; true wavy needs paths
            _ => 1.0,
        };
        frame.fill_rectangle(Point::new(px, y), Size::new(w, h), fg);
        if cell.flags.underline == UnderlineKind::Double {
            frame.fill_rectangle(Point::new(px, y - 2.0), Size::new(w, 1.0), fg);
        }
    }

    // Strikethrough (horizontal line through the middle).
    if cell.flags.strikethrough {
        let y = py + m.cell_h * 0.45;
        frame.fill_rectangle(Point::new(px, y), Size::new(w, 1.0), fg);
    }

    // Overline.
    if cell.flags.overline {
        frame.fill_rectangle(Point::new(px, py + 1.0), Size::new(w, 1.0), fg);
    }
}

fn paint_cursor(frame: &mut Frame, snap: &RenderSnapshot, m: Metrics, fg: Color, bg: Color) {
    if !snap.cursor.visible {
        return;
    }
    let Some((cx, cy)) = snap.cursor.position else {
        return;
    };
    let px = f32::from(cx) * m.cell_w;
    let py = f32::from(cy) * m.cell_h;
    // Block cursor: fill with the foreground, then redraw the glyph under it in
    // the background color so it stays legible (inverse video).
    frame.fill_rectangle(Point::new(px, py), Size::new(m.cell_w, m.cell_h), fg);
    if let Some(cell) = snap.cells.get(cy as usize).and_then(|r| r.get(cx as usize))
        && !cell.grapheme.is_empty()
        && cell.grapheme != " "
        && !cell.flags.invisible
    {
        let base = if cell.width == CellWidth::Wide {
            Metrics::wide_font()
        } else {
            Metrics::narrow_font()
        };
        let font = styled_font(base, &cell.flags);
        frame.fill_text(glyph(&cell.grapheme, px, py, bg, m, font));
    }
}

fn glyph(content: &str, px: f32, py: f32, color: Color, m: Metrics, font: Font) -> Text {
    Text {
        content: content.to_string(),
        position: Point::new(px, py),
        color,
        size: m.font_size.into(),
        font,
        // Basic shaping: no kerning, no ligatures — each grapheme is placed on
        // its own fixed cell, so complex shaping must not shift or merge them.
        shaping: Shaping::Basic,
        ..Text::default()
    }
}

fn color(c: Rgb) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_divides_by_cell_metrics() {
        let m = Metrics::new(15.0);
        let (cols, rows) = m.grid(Size::new(m.cell_w * 80.0, m.cell_h * 24.0));
        assert_eq!((cols, rows), (80, 24));
    }

    #[test]
    fn grid_never_zero() {
        let m = Metrics::new(15.0);
        let (cols, rows) = m.grid(Size::new(1.0, 1.0));
        assert_eq!((cols, rows), (1, 1));
    }

    // ── bug #1: fixed-grid positioning (no glyph-advance creep/overlap) ──

    #[test]
    fn cell_x_is_exactly_col_times_cell_w() {
        let m = Metrics::new(14.0);
        for col in 0u16..200 {
            assert_eq!(m.cell_x(col), col as f32 * m.cell_w);
        }
    }

    #[test]
    fn adjacent_cells_step_by_exactly_one_cell_width() {
        // The invariant that prevents "@f" overlap: consecutive columns are one
        // whole cell apart, independent of which glyphs they hold. (f32 arithmetic
        // isn't bit-exact, so compare within a sub-pixel epsilon.)
        let m = Metrics::new(14.0);
        for col in 0u16..200 {
            let step = m.cell_x(col + 1) - m.cell_x(col);
            assert!(
                (step - m.cell_w).abs() < 1e-2,
                "col {col}: step {step} != {}",
                m.cell_w
            );
        }
    }

    #[test]
    fn col_at_inverts_cell_x_within_the_cell() {
        // A pixel anywhere inside cell `col` maps back to `col`. (Test interior
        // points; the exact boundary is a sub-pixel ambiguity that doesn't matter
        // for click-to-cell.)
        let m = Metrics::new(14.0);
        for col in 0u16..200 {
            let left = m.cell_x(col);
            assert_eq!(m.col_at(left + m.cell_w * 0.5), col); // middle
            assert_eq!(m.col_at(left + m.cell_w * 0.99), col); // just inside the right edge
        }
    }

    #[test]
    fn wide_cells_span_two_columns() {
        assert_eq!(cell_span(CellWidth::Wide), 2.0);
        assert_eq!(cell_span(CellWidth::Narrow), 1.0);
        // The hidden tail of a wide cell is skipped by the caller; counts as 1.
        assert_eq!(cell_span(CellWidth::SpacerTail), 1.0);
    }

    // ── bug #3: absolute-row selection survives scrolling ──

    #[test]
    fn abs_row_and_viewport_row_round_trip() {
        let base = 40;
        let rows = 24;
        for vpy in 0..rows as usize {
            let abs = abs_row(base, vpy);
            assert_eq!(viewport_row(abs, base, rows), Some(vpy));
        }
    }

    #[test]
    fn viewport_row_is_none_outside_the_viewport() {
        let base = 40;
        assert_eq!(viewport_row(39, base, 24), None); // above the top
        assert_eq!(viewport_row(64, base, 24), None); // one past the bottom
        assert_eq!(viewport_row(63, base, 24), Some(23)); // last visible row
    }

    #[test]
    fn selection_anchor_tracks_text_across_scroll() {
        // Press on the text at viewport row 3 while the viewport top is at
        // absolute row 40 (some scroll position). The anchor is stored absolute.
        let anchor = abs_row(40, 3); // = 43
        // Scroll up two lines: the viewport top moves to absolute 42, so that
        // same text is now at viewport row 1 — the highlight follows it, it does
        // not stay pinned to screen row 3.
        assert_eq!(viewport_row(anchor, 42, 24), Some(1));
        // Scroll back down: top at absolute 38 → text now at viewport row 5.
        assert_eq!(viewport_row(anchor, 38, 24), Some(5));
        // The old (buggy) `scroll + vpy` scheme would have moved the highlight
        // the *wrong* way; here the projection is purely `abs - base`.
    }
}
