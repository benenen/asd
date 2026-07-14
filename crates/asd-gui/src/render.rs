//! Draw a [`RenderSnapshot`] onto an iced canvas with a monospace font.
//! M0 scope (spec §7): monospace grid, ASCII correct; CJK/style fidelity is M1.

use asd_vt::{CellWidth, RenderSnapshot, Rgb};
use iced::widget::canvas::{self, Cache, Frame, Geometry, Text};
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
            // Typical monospace advance/line-height ratios.
            cell_w: (font_size * 0.6).round().max(1.0),
            cell_h: (font_size * 1.3).round().max(1.0),
        }
    }

    /// Terminal columns/rows that fit a window of `size` pixels.
    pub fn grid(&self, size: Size) -> (u16, u16) {
        let cols = (size.width / self.cell_w).floor().max(1.0) as u16;
        let rows = (size.height / self.cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }
}

/// Canvas program that paints the current frame from its cache.
pub struct TermCanvas<'a> {
    pub frame: Option<&'a RenderSnapshot>,
    pub cache: &'a Cache,
    pub metrics: Metrics,
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
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            let Some(snap) = self.frame else {
                frame.fill_rectangle(Point::ORIGIN, bounds.size(), Color::BLACK);
                return;
            };
            paint(frame, snap, self.metrics, bounds.size());
        });
        vec![geometry]
    }
}

fn paint(frame: &mut Frame, snap: &RenderSnapshot, m: Metrics, size: Size) {
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
            let span = if cell.width == CellWidth::Wide {
                2.0
            } else {
                1.0
            };

            // Background: only when the cell sets one distinct from the default.
            if let Some(bg) = cell.bg {
                frame.fill_rectangle(
                    Point::new(px, py),
                    Size::new(m.cell_w * span, m.cell_h),
                    color(bg),
                );
            }

            if !cell.grapheme.is_empty() && cell.grapheme != " " {
                let fg = cell.fg.map(color).unwrap_or(default_fg);
                frame.fill_text(glyph(&cell.grapheme, px, py, fg, m));
            }
        }
    }

    paint_cursor(frame, snap, m, default_fg, default_bg);
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
    {
        frame.fill_text(glyph(&cell.grapheme, px, py, bg, m));
    }
}

fn glyph(content: &str, px: f32, py: f32, color: Color, m: Metrics) -> Text {
    Text {
        content: content.to_string(),
        position: Point::new(px, py),
        color,
        size: m.font_size.into(),
        font: Font::MONOSPACE,
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
