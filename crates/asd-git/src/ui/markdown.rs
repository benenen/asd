//! CommonMark-to-terminal rendering. Links and HTML remain inert text.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t'))
                && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

#[derive(Default)]
struct Builder {
    rows: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    style: Style,
    lists: Vec<Option<u64>>,
    links: Vec<String>,
    quote: usize,
    code: bool,
}

impl Builder {
    fn text(&mut self, text: &str) {
        for (i, part) in safe(text).split('\n').enumerate() {
            if i > 0 {
                self.flush(true);
            }
            if !part.is_empty() {
                if self.spans.is_empty() && self.quote > 0 {
                    self.spans.push(Span::styled(
                        "│ ".repeat(self.quote),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                self.spans.push(Span::styled(part.to_owned(), self.style));
            }
        }
    }
    fn flush(&mut self, force: bool) {
        if force || !self.spans.is_empty() {
            self.rows.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }
    fn blank(&mut self) {
        self.flush(false);
        if self.rows.last().is_some_and(|line| !line.spans.is_empty()) {
            self.rows.push(Line::default());
        }
    }
    fn start(&mut self, tag: Tag<'_>) {
        self.styles.push(self.style);
        match tag {
            Tag::Heading { .. } => {
                self.blank();
                self.style = self.style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
            }
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::CodeBlock(_) => {
                self.blank();
                self.code = true;
                self.style = self.style.fg(Color::Yellow).bg(Color::Rgb(35, 40, 48));
            }
            Tag::BlockQuote(_) => {
                self.blank();
                self.quote += 1;
                self.style = self.style.fg(Color::Gray);
            }
            Tag::List(start) => {
                self.flush(false);
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush(false);
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                let bullet = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let text = format!("{number}. ");
                        *number = number.saturating_add(1);
                        text
                    }
                    _ => "• ".into(),
                };
                self.text(&format!("{indent}{bullet}"));
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                self.style = self
                    .style
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED);
            }
            Tag::TableHead | Tag::TableRow => self.flush(false),
            Tag::TableCell => self.text("│ "),
            _ => {}
        }
    }
    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) | TagEnd::Paragraph => self.blank(),
            TagEnd::CodeBlock => {
                self.code = false;
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush(false);
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::Item => self.flush(false),
            TagEnd::List(_) => {
                self.lists.pop();
                self.blank();
            }
            TagEnd::Link | TagEnd::Image => {
                if let Some(url) = self.links.pop() {
                    self.text(&format!(" ({url})"));
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => self.flush(false),
            TagEnd::TableCell => self.text(" "),
            _ => {}
        }
        self.style = self.styles.pop().unwrap_or_default();
    }
}

pub(crate) fn lines(source: &str) -> Vec<Line<'static>> {
    let mut builder = Builder::default();
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => builder.start(tag),
            Event::End(tag) => builder.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => builder.text(&text),
            Event::Code(text) => {
                let saved = builder.style;
                builder.style = saved.fg(Color::Yellow).bg(Color::Rgb(35, 40, 48));
                builder.text(&text);
                builder.style = saved;
            }
            Event::SoftBreak if !builder.code => builder.text(" "),
            Event::SoftBreak | Event::HardBreak => builder.flush(true),
            Event::Rule => {
                builder.blank();
                builder.text("────────");
                builder.blank();
            }
            Event::TaskListMarker(done) => builder.text(if done { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(text) => builder.text(&format!("[{text}]")),
            Event::InlineMath(text) | Event::DisplayMath(text) => builder.text(&text),
        }
    }
    builder.flush(false);
    while builder
        .rows
        .last()
        .is_some_and(|line| line.spans.is_empty())
    {
        builder.rows.pop();
    }
    builder.rows
}

/// Soft wrap by grapheme/cell width, retaining styles and every scrollable row.
pub(crate) fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut output = Vec::new();
    for line in lines {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0;
        for span in line.spans {
            let content = safe(&span.content).replace('\t', "    ");
            for grapheme in content.graphemes(true) {
                let size = grapheme.width();
                if col + size > usize::from(width) && !spans.is_empty() {
                    output.push(Line::from(std::mem::take(&mut spans)));
                    col = 0;
                }
                let text = if size > usize::from(width) {
                    "…"
                } else {
                    grapheme
                };
                if let Some(last) = spans.last_mut().filter(|last| last.style == span.style) {
                    last.content.to_mut().push_str(text);
                } else {
                    spans.push(Span::styled(text.to_owned(), span.style));
                }
                col += text.width();
            }
        }
        output.push(Line::from(spans));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn commonmark_styles_and_entities_remain_readable() {
        let rows = lines(
            "## Header\n\n- **bold** and *italic* `code` &lt;x&gt;\n\n> quote\n\n```rust\nlet x = 1;\n```\n\n[docs](https://example.invalid)",
        );
        let out = text(&rows);
        for expected in [
            "Header",
            "• bold and italic code <x>",
            "│ quote",
            "let x = 1;",
            "docs (https://example.invalid)",
        ] {
            assert!(out.contains(expected), "{out}");
        }
        assert!(
            rows.iter().flat_map(|l| &l.spans).any(
                |s| s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD)
            )
        );
        assert!(
            rows.iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains("code") && s.style.bg.is_some())
        );
    }
    #[test]
    fn narrow_wrapping_preserves_wide_graphemes_and_strips_controls() {
        let rows = wrap(lines("中文字符 **hello**\n\nend\u{1b}\u{7}\u{202e}"), 4);
        assert!(rows.iter().all(|l| l.width() <= 4));
        assert!(text(&rows).contains("中文"));
        assert!(!text(&rows).contains('\u{1b}'));
        assert!(!text(&rows).contains('\u{202e}'));
    }
}
