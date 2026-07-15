//! Draw a [`RenderSnapshot`] onto an iced canvas with a monospace font.
//! M0 scope (spec §7): monospace grid, ASCII correct; CJK/style fidelity is M1.

use asd_vt::{CellSnapshot, CellWidth, RenderSnapshot, Rgb, StyleFlags, UnderlineKind};
use iced::widget::canvas::{self, Cache, Frame, Geometry, Text};

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

    /// Primary monospace font (narrow cells). JetBrains Mono is Ghostty's
    /// recommended terminal font — crisp, well-hinted, with excellent box-
    /// drawing / powerline glyph coverage. Falls back to system monospace
    /// when not installed.
    pub fn narrow_font() -> Font {
        Font::with_name("JetBrains Mono")
    }

    /// Font for wide (CJK) cells. Tries PingFang SC on macOS (which has
    /// excellent CJK coverage in a clean sans-serif), falling back to the
    /// system default when unavailable.
    pub fn wide_font() -> Font {
        Font::with_name("PingFang SC")
    }

    /// Terminal columns/rows that fit a window of `size` pixels.
    pub fn grid(&self, size: Size) -> (u16, u16) {
        let cols = (size.width / self.cell_w).floor().max(1.0) as u16;
        let rows = (size.height / self.cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }
}

/// Canvas program that paints the current frame, highlights the active
/// selection, and emits messages for mouse wheel / drag events.
pub struct TermCanvas<'a> {
    pub frame: Option<&'a RenderSnapshot>,
    pub cache: &'a Cache,
    pub metrics: Metrics,
    pub selection: Option<crate::GuiSelection>,
    pub scroll: usize,
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
        let scroll = self.scroll;
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            let Some(snap) = self.frame else {
                frame.fill_rectangle(Point::ORIGIN, bounds.size(), Color::BLACK);
                return;
            };
            paint(frame, snap, self.metrics, bounds.size(), sel, scroll);
        });
        vec![geometry]
    }
}

fn paint(frame: &mut Frame, snap: &RenderSnapshot, m: Metrics, size: Size, sel: Option<crate::GuiSelection>, scroll: usize) {
    let default_bg = color(snap.background);
    let default_fg = color(snap.foreground);
    frame.fill_rectangle(Point::ORIGIN, size, default_bg);

    for (y, row) in snap.cells.iter().enumerate() {
        let py = y as f32 * m.cell_h;
        for (x, cell) in row.iter().enumerate() {
            if cell.width == CellWidth::SpacerTail {
                continue;
            }
            let px = x as f32 * m.cell_w;
            let span: f32 = if cell.width == CellWidth::Wide {
                2.0
            } else {
                1.0
            };

            // Selection highlight: convert screen-space to viewport,
            // matching the CLI's content-pin selection behavior.
            let selected = !cell.grapheme.is_empty()
                && sel.is_some_and(|s| {
                    let (a, b) = if (s.anchor.1, s.anchor.0) <= (s.head.1, s.head.0) {
                        (s.anchor, s.head)
                    } else {
                        (s.head, s.anchor)
                    };
                    // Project screen rows to viewport rows.
                    let vpy = y as usize;
                    let screen_y = scroll + vpy;
                    screen_y >= a.1 && screen_y <= b.1
                        && (x as u16) >= if screen_y == a.1 { a.0 } else { 0 }
                        && (x as u16) <= if screen_y == b.1 { b.0 } else { snap.cols.saturating_sub(1) as u16 }
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
        weight: if flags.bold { Weight::Bold } else { base.weight },
        style: if flags.italic { Style::Italic } else { base.style },
        ..base
    }
}

/// Draw underline / strikethrough / overline lines for a cell.
fn paint_decorations(frame: &mut Frame, cell: &CellSnapshot, px: f32, py: f32, m: Metrics, fg: Color) {
    let w = if cell.width == CellWidth::Wide { m.cell_w * 2.0 } else { m.cell_w };

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
}
