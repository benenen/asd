//! Syntax highlighting for the file diff view.
//!
//! syntect owns the parsing; this maps its colours onto ratatui styles and
//! keeps the per-file parse state. That state is the point: a line inside a
//! block comment or a multi-line string is only coloured correctly when the
//! parser carried state forward from the lines above it, so one
//! [`HighlightLines`] lives for as long as the file being highlighted and is
//! thrown away only by `Highlighter::reset` or by the path changing.
//!
//! The default syntax set and theme are deserialised once per process and
//! shared, because `asd ui` paints every open session from the thread that
//! calls in here.
//!
//! Cost, measured on this machine against a 1295-line Rust file: loading both
//! dumps is 2.6 ms in release and 40 ms in debug, but highlighting itself is
//! ~141 us per line in release and ~1.7 ms per line in debug — 23 ms and
//! 200 ms respectively for one 60-line screenful. Highlighting is therefore
//! the expensive part, not the loading, and a caller must run it once when a
//! file diff arrives rather than once per frame.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// The theme whose colours the diff view borrows. `asd`'s own palette is dark
/// and RGB throughout, so a dark RGB theme sits on top of it without clashing.
const THEME_NAME: &str = "base16-ocean.dark";

/// Deserialised once per process, not once per [`Highlighter`]: several
/// sessions may each own a git overlay, and this dump is the expensive part.
static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

/// The shared syntax set, built on first use.
///
/// The "newlines" variant is the one syntect documents for line-by-line use,
/// so every line handed to it carries a trailing newline that
/// [`Highlighter::line`] appends and strips back off again. Compared against
/// `load_defaults_nonewlines` over 2494 lines of this crate's own Rust the two
/// agreed on every line, so this is the documented path rather than a measured
/// improvement; syntaxes whose rules match the newline explicitly need it.
fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The shared theme, built on first use.
fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        // Never index the map: a missing theme would panic, and this runs on
        // the thread that paints every session.
        ThemeSet::load_defaults()
            .themes
            .remove(THEME_NAME)
            .unwrap_or_default()
    })
}

/// Highlights lines of one file at a time.
///
/// Feed it a file's lines in order. It keeps syntect's parse state between
/// calls so multi-line constructs continue correctly, and starts over when the
/// path changes or [`reset`](Self::reset) is called.
pub(crate) struct Highlighter {
    /// The file currently being highlighted, and its parse state. The `'static`
    /// is real rather than a placeholder: [`HighlightLines`] borrows only the
    /// theme, and the theme lives in a `static` rather than in this struct, so
    /// nothing here is self-referential.
    current: Option<(String, HighlightLines<'static>)>,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // HighlightLines is not Debug, so report only which file is in progress.
        f.debug_struct("Highlighter")
            .field("current", &self.current.as_ref().map(|(p, _)| p))
            .finish_non_exhaustive()
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    /// Build a highlighter, warming the shared syntax set and theme.
    ///
    /// The first one in the process pays for both dumps (2.6 ms in release,
    /// 40 ms in debug); every later one is free.
    pub(crate) fn new() -> Self {
        let _ = syntaxes();
        let _ = theme();
        Self { current: None }
    }

    /// Forget the previous file's parse state. syntect carries state across
    /// lines, so without this a file that opens a block comment would leave
    /// the next file's first line inside it — including when the next file is
    /// the same path at a different commit, which a path check cannot catch.
    pub(crate) fn reset(&mut self) {
        self.current = None;
    }

    /// Highlight one line of `path`, returning styled spans whose text
    /// concatenates back to the input exactly.
    ///
    /// `text` is one line without its terminator. The round-trip is checked,
    /// not assumed: anything that would drop characters degrades to a single
    /// unstyled span, which is also what a path with no known syntax gives.
    pub(crate) fn line(&mut self, path: &str, text: &str) -> Vec<(Style, String)> {
        let syntaxes = syntaxes();
        let Some(syntax) = syntax_for(syntaxes, path) else {
            // Drop any state so returning to a highlighted file restarts it
            // rather than resuming where that file left off.
            self.current = None;
            return vec![(Style::default(), text.to_string())];
        };

        // Rebuild the per-file state when the path changes; otherwise keep the
        // existing one so this line continues where the last one ended.
        let restart = match &self.current {
            Some((p, _)) => p != path,
            None => true,
        };
        if restart {
            self.current = Some((path.to_string(), HighlightLines::new(syntax, theme())));
        }
        let Some((_, hl)) = self.current.as_mut() else {
            return vec![(Style::default(), text.to_string())];
        };

        // syntect's default definitions expect the newline, and several of
        // them only close a construct when they see it.
        let mut owned = String::with_capacity(text.len() + 1);
        owned.push_str(text);
        owned.push('\n');

        let Ok(ranges) = hl.highlight_line(&owned, syntaxes) else {
            // A parse failure poisons the state for the rest of the file, so
            // give up on this file rather than colouring the rest from it.
            self.current = None;
            return vec![(Style::default(), text.to_string())];
        };

        let spans = ranges
            .into_iter()
            .map(|(style, piece)| (convert(style), piece.to_string()))
            .collect();
        match finish(spans, text) {
            Some(spans) => spans,
            None => {
                // A truncated partition leaves the highlight state stranded
                // mid-line while the parse state reached the end of it, so it
                // is no longer usable for the rest of the file.
                self.current = None;
                vec![(Style::default(), text.to_string())]
            }
        }
    }
}

/// Tidy syntect's ranges into spans, or refuse them.
///
/// Returns `None` when the spans do not reproduce `text`, which the caller
/// must treat as "this line cannot be highlighted" rather than paint.
///
/// The check is not paranoia about arithmetic. `RangedHighlightIterator::next`
/// ends the iteration early on a `ScopeStackOp::Restore` with an empty clear
/// stack (`highlighter.rs`, the `.ok()?`) while `parse_line` still returns
/// `Ok`, so the partition can come back covering only part of the line. Without
/// this the trailing newline would simply not be found, no pop would happen,
/// and a diff row would be painted with characters missing and nothing logged.
///
/// Comparing lengths is enough: the spans are always a prefix of the input, so
/// equal lengths mean equal text.
fn finish(mut spans: Vec<(Style, String)>, text: &str) -> Option<Vec<(Style, String)>> {
    // Take back the newline `line` appended. highlight_line partitions its
    // input, so it is the last byte of the last span.
    if let Some((_, last)) = spans.last_mut()
        && last.ends_with('\n')
    {
        last.pop();
    }
    // Empty spans carry nothing and would only make callers loop further.
    spans.retain(|(_, t)| !t.is_empty());

    let covered: usize = spans.iter().map(|(_, t)| t.len()).sum();
    if covered != text.len() {
        return None;
    }
    if spans.is_empty() {
        spans.push((Style::default(), String::new()));
    }
    Some(spans)
}

/// The syntax for a path, by extension and then by whole file name.
///
/// syntect files whole names in the same `file_extensions` list as real
/// extensions — `Makefile`, `Rakefile`, `Gemfile`, `Vagrantfile`, `.bashrc`,
/// `config.ru` — and `Path::extension` returns `None` for every one of them,
/// so the name is tried whenever the extension misses rather than only when
/// there is no extension. That also picks up `config.ru`, whose extension
/// (`ru`) is not registered but whose full name is.
///
/// Content is never sniffed: the diff view has only the lines it is asked
/// about, not the file, so `find_syntax_by_first_line` has nothing to read.
fn syntax_for<'a>(syntaxes: &'a SyntaxSet, path: &str) -> Option<&'a SyntaxReference> {
    let path = std::path::Path::new(path);
    let by_extension = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| syntaxes.find_syntax_by_extension(e));
    by_extension.or_else(|| {
        path.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| syntaxes.find_syntax_by_extension(n))
    })
}

/// syntect colours are RGBA; the terminal gets the RGB.
///
/// Only the foreground is taken. syntect's background would paint over the
/// pane's own and fight the selected-row highlight.
fn convert(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spans' text, which must always be the input again.
    fn joined(spans: &[(Style, String)]) -> String {
        spans.iter().map(|(_, t)| t.as_str()).collect()
    }

    #[test]
    fn a_rust_line_is_split_into_more_than_one_span() {
        let mut h = Highlighter::new();
        let spans = h.line("a.rs", "fn main() { let x = 1; }");
        assert!(spans.len() > 1, "expected several spans, got {spans:?}");
        assert_eq!(
            joined(&spans),
            "fn main() { let x = 1; }",
            "text must round-trip"
        );
    }

    #[test]
    fn an_unknown_extension_returns_the_line_unstyled_in_one_span() {
        let mut h = Highlighter::new();
        let spans = h.line("notes.zzzz", "some text");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1, "some text");
        assert_eq!(
            spans[0].0,
            Style::default(),
            "an unknown syntax must not invent a colour"
        );
    }

    #[test]
    fn a_path_with_no_syntax_at_all_is_unstyled_rather_than_a_panic() {
        // Degenerate paths reach here from the diff worker, and the render
        // thread paints every session, so none of these may panic.
        let mut h = Highlighter::new();
        for path in ["LICENSE", "", "dir/", ".", "..", "/", "notes.zzzz"] {
            let spans = h.line(path, "some text");
            assert_eq!(spans.len(), 1, "{path:?} gave {spans:?}");
            assert_eq!(joined(&spans), "some text");
            assert_eq!(spans[0].0, Style::default(), "{path:?} invented a colour");
        }
    }

    #[test]
    fn a_file_known_by_name_rather_than_by_extension_is_still_highlighted() {
        // syntect keeps whole file names in the same list as real extensions,
        // and `Path::extension` is None for every one of them, so looking at
        // the extension alone leaves these unstyled.
        let mut h = Highlighter::new();
        for (path, line) in [
            ("Makefile", "CFLAGS = -O2"),
            ("dir/Makefile", "CFLAGS = -O2"),
            ("Gemfile", "gem \"rails\""),
            (".bashrc", "export PATH=\"$HOME/bin\""),
            // Extension present but unregistered; the name still resolves.
            ("config.ru", "run Rails.application"),
        ] {
            h.reset();
            let spans = h.line(path, line);
            assert!(spans.len() > 1, "{path} was not highlighted: {spans:?}");
            assert_eq!(joined(&spans), line, "round-trip failed for {path}");
        }
    }

    #[test]
    fn a_truncated_partition_degrades_to_plain_text_rather_than_dropping_characters() {
        // `RangedHighlightIterator::next` gives up early on a `Restore` with an
        // empty clear stack while `parse_line` still returns `Ok`, which hands
        // back a partition covering only part of the line. It cannot be
        // provoked through this API with syntect's bundled syntaxes -- it was
        // found by reading syntect, not by triggering it -- so the guard is
        // exercised directly rather than through a manufactured input.
        let short = vec![(Style::default(), "fn ma".to_string())];
        assert_eq!(
            finish(short, "fn main() {}"),
            None,
            "a short partition must be refused, not painted"
        );

        let whole = vec![
            (Style::default(), "fn ".to_string()),
            (Style::default(), "main() {}\n".to_string()),
        ];
        let kept = finish(whole, "fn main() {}").expect("a full partition is kept");
        assert_eq!(joined(&kept), "fn main() {}");

        // The empty line: nothing but the appended newline comes back.
        let only_newline = vec![(Style::default(), "\n".to_string())];
        let kept = finish(only_newline, "").expect("an empty line is not a truncation");
        assert_eq!(kept.len(), 1);
        assert_eq!(joined(&kept), "");
    }

    #[test]
    fn text_always_round_trips_whatever_the_syntax() {
        let mut h = Highlighter::new();
        for (path, line) in [
            ("a.rs", "    let s = \"quoted\"; // trailing"),
            ("b.toml", "key = \"value\""),
            ("c.md", "# heading"),
            ("d.json", "{\"a\": 1}"),
            ("weird.rs", ""),
            ("cjk.rs", "let s = \"中文字符\";"),
            // Trailing whitespace is meaningful in a diff and is exactly what
            // a newline-stripping bug would eat.
            ("trailing.rs", "let x = 1;   "),
            ("tabs.go", "\tif err != nil {"),
            ("emoji.md", "- 🚀 ship it 🚀"),
            ("blank.py", "   "),
            ("crlf.rs", "let x = 1;\r"),
            ("unknown.zzzz", "  中文 with trailing  "),
        ] {
            h.reset();
            let spans = h.line(path, line);
            assert_eq!(joined(&spans), line, "round-trip failed for {path}");
            assert!(!spans.is_empty(), "no spans at all for {path}");
        }
    }

    #[test]
    fn only_an_empty_line_produces_an_empty_span() {
        // Task 10 walks these to place columns; an empty span is a wasted
        // iteration and a zero-width write. An empty line is the one case
        // that has nothing else to return.
        let mut h = Highlighter::new();
        for line in ["", "fn main() {}", "   ", "let s = \"中文\";"] {
            let spans = h.line("a.rs", line);
            let empties = spans.iter().filter(|(_, t)| t.is_empty()).count();
            if line.is_empty() {
                // An empty line is the one case that has nothing to carry.
                assert_eq!(spans.len(), 1);
            } else {
                assert_eq!(empties, 0, "{line:?} produced empty spans: {spans:?}");
            }
        }
    }

    #[test]
    fn parse_state_carries_across_lines_within_a_file() {
        // This is what a per-line HighlightLines throws away: the second line
        // is only known to be inside the comment because the first one opened
        // it. Recreating the parser per line makes `inside` equal `outside`.
        let mut h = Highlighter::new();
        h.line("a.rs", "/* opened");
        let inside = h.line("a.rs", "fn main() {}");

        let mut fresh = Highlighter::new();
        let outside = fresh.line("a.rs", "fn main() {}");

        assert!(
            outside.len() > 1,
            "outside a comment this line has several spans: {outside:?}"
        );
        assert_eq!(
            inside.len(),
            1,
            "inside a block comment the whole line is one span: {inside:?}"
        );
        assert_ne!(
            inside, outside,
            "parse state did not carry from the line that opened the comment"
        );
        assert_eq!(joined(&inside), "fn main() {}");
    }

    #[test]
    fn a_multi_line_string_keeps_its_colour_on_the_second_line() {
        // The same property through a different construct, so a comment-only
        // quirk of one syntax cannot be the whole evidence.
        let mut h = Highlighter::new();
        h.line("a.rs", "let s = \"opened");
        let inside = h.line("a.rs", "fn main() {}");

        let mut fresh = Highlighter::new();
        let outside = fresh.line("a.rs", "fn main() {}");

        assert_ne!(
            inside, outside,
            "the unterminated string did not carry to the next line"
        );
        assert_eq!(joined(&inside), "fn main() {}");
    }

    #[test]
    fn a_closed_comment_returns_to_normal_highlighting() {
        // Carrying state is only half of it. A highlighter that latched the
        // comment colour and never let go would pass the test above; this one
        // needs the parser to leave the comment when it ends.
        let mut h = Highlighter::new();
        h.line("a.rs", "/* opened");
        h.line("a.rs", "still inside");
        h.line("a.rs", "closed */");
        let after = h.line("a.rs", "fn main() {}");

        let mut fresh = Highlighter::new();
        assert_eq!(
            after,
            fresh.line("a.rs", "fn main() {}"),
            "the block comment never closed"
        );
    }

    #[test]
    fn reset_starts_a_new_file_rather_than_continuing_the_last() {
        // Syntect carries parse state across lines. A file that opens a block
        // comment must not leave the next file's first line inside it.
        //
        // The path deliberately does NOT change: `line` restarts by itself on
        // a new path, which would hide a `reset` that does nothing. The same
        // path at a different commit is also the real case reset exists for.
        let mut h = Highlighter::new();
        h.line("a.rs", "/* opened");
        h.reset();
        let spans = h.line("a.rs", "fn main() {}");

        let mut fresh = Highlighter::new();
        assert_eq!(
            spans,
            fresh.line("a.rs", "fn main() {}"),
            "state leaked from the previous file: {spans:?}"
        );
        assert!(
            spans.len() > 1,
            "state leaked from the previous file: {spans:?}"
        );
    }

    #[test]
    fn a_new_path_starts_a_new_file_too() {
        let mut h = Highlighter::new();
        h.line("a.rs", "/* opened");
        let spans = h.line("b.rs", "fn main() {}");

        let mut fresh = Highlighter::new();
        assert_eq!(spans, fresh.line("b.rs", "fn main() {}"));
    }

    #[test]
    fn an_unknown_file_in_between_does_not_leave_the_old_state_to_resume() {
        let mut h = Highlighter::new();
        h.line("a.rs", "/* opened");
        h.line("notes.zzzz", "plain");
        let spans = h.line("a.rs", "fn main() {}");

        let mut fresh = Highlighter::new();
        assert_eq!(
            spans,
            fresh.line("a.rs", "fn main() {}"),
            "the comment state survived a detour through an unknown file"
        );
    }

    #[test]
    fn every_span_is_an_rgb_colour() {
        // Named ANSI colours would follow the host terminal's theme and clash
        // with asd's own palette, which is RGB throughout.
        let mut h = Highlighter::new();
        let spans = h.line("a.rs", "fn main() { let x = 1; }");
        for (style, text) in &spans {
            assert!(
                matches!(style.fg, Some(Color::Rgb(..))),
                "{text:?} got {:?}",
                style.fg
            );
        }
    }

    #[test]
    fn the_theme_is_the_one_we_asked_for() {
        // `unwrap_or_default` would silently fall back to a themeless theme,
        // which paints every span the same colour.
        assert_eq!(theme().name.as_deref(), Some("Base16 Ocean Dark"));
        assert!(!theme().scopes.is_empty());
    }
}
