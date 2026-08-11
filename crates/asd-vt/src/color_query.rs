//! Streaming removal of OSC 10/11 queries from client-facing PTY output.

#[derive(Clone, Copy)]
pub(crate) enum ColorQuery {
    Foreground,
    Background,
}

impl ColorQuery {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Foreground => 10,
            Self::Background => 11,
        }
    }

    pub(crate) fn response_prefixes(self) -> [&'static [u8]; 2] {
        match self {
            Self::Foreground => [b"\x1b]10;", b"\x9d10;"],
            Self::Background => [b"\x1b]11;", b"\x9d11;"],
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OscTerminator {
    Bel,
    St,
    C1St,
}

impl OscTerminator {
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Bel => b"\x07",
            Self::St => b"\x1b\\",
            Self::C1St => b"\x9c",
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) enum ColorQueryScan {
    #[default]
    Ground,
    Esc,
    Osc,
    OscOne,
    Code(ColorQuery),
    Semicolon(ColorQuery),
    Question(ColorQuery),
    QuestionEsc(ColorQuery),
}

impl ColorQueryScan {
    pub(crate) fn push(&mut self, byte: u8) -> Option<(ColorQuery, OscTerminator)> {
        use ColorQueryScan::{Code, Esc, Ground, Osc, OscOne, Question, QuestionEsc, Semicolon};

        let previous = std::mem::take(self);
        let (next, response) = match (previous, byte) {
            (Ground, b'\x1b') => (Esc, None),
            (Ground, b'\x9d') => (Osc, None),
            (Esc, b']') => (Osc, None),
            (Osc, b'1') => (OscOne, None),
            (OscOne, b'0') => (Code(ColorQuery::Foreground), None),
            (OscOne, b'1') => (Code(ColorQuery::Background), None),
            (Code(query), b';') => (Semicolon(query), None),
            (Semicolon(query), b'?') => (Question(query), None),
            (Question(query), b'\x07') => (Ground, Some((query, OscTerminator::Bel))),
            (Question(query), b'\x9c') => (Ground, Some((query, OscTerminator::C1St))),
            (Question(query), b'\x1b') => (QuestionEsc(query), None),
            (QuestionEsc(query), b'\\') => (Ground, Some((query, OscTerminator::St))),
            // The ESC that led to QuestionEsc can also start a new OSC.
            (QuestionEsc(_), b']') => (Osc, None),
            (_, b'\x9d') => (Osc, None),
            (_, b'\x1b') => (Esc, None),
            _ => (Ground, None),
        };
        *self = next;
        response
    }
}

const QUERIES: [&[u8]; 12] = [
    b"\x1b]10;?\x07",
    b"\x1b]10;?\x1b\\",
    b"\x1b]10;?\x9c",
    b"\x1b]11;?\x07",
    b"\x1b]11;?\x1b\\",
    b"\x1b]11;?\x9c",
    b"\x9d10;?\x07",
    b"\x9d10;?\x1b\\",
    b"\x9d10;?\x9c",
    b"\x9d11;?\x07",
    b"\x9d11;?\x1b\\",
    b"\x9d11;?\x9c",
];

/// Holds only a possible query prefix between chunks. Complete OSC 10/11
/// queries are removed; every mismatch is released byte-for-byte.
#[derive(Debug, Default)]
pub struct ColorQueryFilter {
    pending: Vec<u8>,
}

impl ColorQueryFilter {
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len());
        for &byte in input {
            self.pending.push(byte);
            loop {
                if QUERIES.iter().any(|query| query.starts_with(&self.pending)) {
                    if QUERIES.iter().any(|query| *query == self.pending) {
                        self.pending.clear();
                    }
                    break;
                }
                output.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        output
    }

    /// Release an incomplete prefix when the PTY stream ends.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color_queries_across_chunks_without_eating_neighbors() {
        let mut filter = ColorQueryFilter::default();
        let mut output = Vec::new();

        output.extend(filter.push(b"before\x1b]10;"));
        output.extend(filter.push(b"?\x1b\\middle\x1b]11;?"));
        output.extend(filter.push(b"\x07after\x1b]12;?\x07"));

        assert_eq!(output, b"beforemiddleafter\x1b]12;?\x07");
    }

    #[test]
    fn strips_c1_and_seven_bit_queries_across_every_chunk_boundary() {
        let introducers: [&[u8]; 2] = [b"\x1b]", b"\x9d"];
        let terminators: [&[u8]; 3] = [b"\x07", b"\x1b\\", b"\x9c"];

        for introducer in introducers {
            for terminator in terminators {
                for code in [b"10".as_slice(), b"11".as_slice()] {
                    let mut query = introducer.to_vec();
                    query.extend_from_slice(code);
                    query.extend_from_slice(b";?");
                    query.extend_from_slice(terminator);

                    for split in 0..=query.len() {
                        let mut filter = ColorQueryFilter::default();
                        let mut output = b"before".to_vec();
                        output.extend(filter.push(&query[..split]));
                        output.extend(filter.push(&query[split..]));
                        output.extend(filter.push(b"after"));

                        assert_eq!(output, b"beforeafter", "query={query:?}, split={split}");
                        assert!(filter.finish().is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn finish_releases_an_incomplete_query_prefix() {
        let mut filter = ColorQueryFilter::default();

        assert_eq!(filter.push(b"before\x1b]11;"), b"before");
        assert_eq!(filter.finish(), b"\x1b]11;");
        assert!(filter.finish().is_empty());
    }
}
