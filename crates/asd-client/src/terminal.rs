//! Host-terminal color probing shared by the terminal clients.

use std::time::Duration;

use asd_proto::{TerminalAppearance, TerminalColor};

/// Query both host-terminal defaults. Replies arrive on stdin while it is in
/// raw mode and are removed before any simultaneously typed bytes are handed
/// to the attached session.
pub(crate) const COLOR_QUERY: &[u8] = b"\x1b]10;?\x07\x1b]11;?\x07";
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(150);
pub(crate) const PROBE_LATE_GRACE: Duration = Duration::from_millis(150);
pub(crate) const PROBE_PASTE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_PROBE_BYTES: usize = 512;
/// Once a real mode-2004 paste starts, keep the whole event together up to the
/// largest payload the client can forward in one protocol frame.
pub(crate) const MAX_PROBE_PASTE_BYTES: usize = asd_proto::MAX_FRAME_LEN - 1024;
pub(crate) const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub(crate) const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProbeResult {
    pub appearance: TerminalAppearance,
    /// Non-reply bytes read while probing, typically keys typed during the
    /// short startup window.
    pub input: Vec<u8>,
}

/// Probe the real terminal attached to stdin/stdout. Call only after enabling
/// raw input and before starting any other stdin reader.
pub fn probe_terminal_colors() -> std::io::Result<ProbeResult> {
    crate::platform::probe_terminal_colors()
}

/// Remove complete OSC 10/11 RGB replies from `bytes`, returning the colors
/// they reported. Everything else remains in order for the caller to forward
/// as ordinary input. Incomplete replies remain buffered for the next call;
/// [`finish_probe_input`] removes them at the bounded end of a one-shot probe.
pub fn extract_terminal_replies(bytes: &mut Vec<u8>) -> TerminalAppearance {
    let mut appearance = TerminalAppearance::default();
    while let Some(reply) = find_reply(bytes) {
        if let Some(color) = parse_rgb(&bytes[reply.body.clone()]) {
            appearance = match reply.code {
                10 => TerminalAppearance {
                    foreground: Some(color),
                    ..appearance
                },
                11 => TerminalAppearance {
                    background: Some(color),
                    ..appearance
                },
                _ => unreachable!(),
            };
        }
        bytes.drain(reply.full);
    }
    appearance
}

/// Whether a terminal reply has started but has not reached BEL/ST yet. The
/// platform probe uses this to grant a bounded grace period when a reply is
/// split exactly at the normal timeout.
pub(crate) fn has_incomplete_terminal_reply(bytes: &[u8]) -> bool {
    incomplete_reply_start(bytes).is_some()
}

/// Whether host mode 2004 marked the beginning of a paste whose end has not
/// arrived yet. The platform probe must not return halfway through such an
/// event or its tail could later bypass the target session's paste handling.
pub(crate) fn has_incomplete_bracketed_paste(bytes: &[u8]) -> bool {
    let mut remaining = bytes;
    while let Some(start) = find_bytes(remaining, BRACKETED_PASTE_START) {
        let content = &remaining[start + BRACKETED_PASTE_START.len()..];
        let Some(end) = find_bytes(content, BRACKETED_PASTE_END) else {
            return true;
        };
        remaining = &content[end + BRACKETED_PASTE_END.len()..];
    }
    false
}

/// Remove a terminal-protocol fragment that cannot safely become session
/// input. Bytes before the fragment (keys typed during the probe) are kept.
pub(crate) fn finish_probe_input(bytes: &mut Vec<u8>) {
    while let Some(start) = incomplete_reply_start(bytes) {
        let keep_from = start + terminal_reply_prefix_len(&bytes[start..]);
        bytes.drain(start..keep_from);
    }
}

/// Replay input captured before the client knew whether the session used
/// bracketed paste. The probe temporarily enables host mode 2004, so only
/// input carrying a real host paste envelope is treated as a paste; ordinary
/// Enter keypresses remain ordinary input.
pub fn prepare_probe_input(input: Vec<u8>, bracketed: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut remaining = input.as_slice();
    while let Some(start) = find_bytes(remaining, BRACKETED_PASTE_START) {
        output.extend_from_slice(&remaining[..start]);
        let content = &remaining[start + BRACKETED_PASTE_START.len()..];
        if let Some(end) = find_bytes(content, BRACKETED_PASTE_END) {
            output.extend_from_slice(&paste_bytes(&content[..end], bracketed));
            remaining = &content[end + BRACKETED_PASTE_END.len()..];
        } else {
            // The probe ended mid-paste. The start marker still proves event
            // identity, so preserve the captured content without leaking a
            // host-only marker into a non-2004 session.
            output.extend_from_slice(&paste_bytes(content, bracketed));
            remaining = &[];
        }
    }
    output.extend_from_slice(remaining);
    output
}

/// Encode one known paste event for the target session. Embedded end markers
/// are removed so pasted content cannot close the envelope early.
pub fn paste_bytes(input: &[u8], bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return input.to_vec();
    }
    let mut output =
        Vec::with_capacity(input.len() + BRACKETED_PASTE_START.len() + BRACKETED_PASTE_END.len());
    output.extend_from_slice(BRACKETED_PASTE_START);
    let mut remaining = input;
    while let Some(at) = find_bytes(remaining, BRACKETED_PASTE_END) {
        output.extend_from_slice(&remaining[..at]);
        remaining = &remaining[at + BRACKETED_PASTE_END.len()..];
    }
    output.extend_from_slice(remaining);
    output.extend_from_slice(BRACKETED_PASTE_END);
    output
}

/// Length of the syntactically possible OSC 10/11 RGB reply prefix. Once a
/// byte cannot belong to that grammar it is independent user input and must
/// survive the probe even if the reply never supplied BEL/ST.
fn terminal_reply_prefix_len(bytes: &[u8]) -> usize {
    let (marker, marker_len) = REPLY_MARKERS
        .iter()
        .map(|(marker, _)| (*marker, common_prefix_len(bytes, marker)))
        .max_by_key(|(_, matched)| *matched)
        .unwrap_or((&[], 0));
    if marker_len < marker.len() {
        return marker_len;
    }
    if marker_len == bytes.len() {
        return marker_len;
    }

    let body = &bytes[marker_len..];
    let rgb = b"rgb:";
    let shared = body.len().min(rgb.len());
    if body[..shared] != rgb[..shared] {
        return marker_len;
    }
    if body.len() <= rgb.len() {
        return marker_len + body.len();
    }

    let mut index = rgb.len();
    for channel in 0..3 {
        let start = index;
        while index < body.len() && body[index].is_ascii_hexdigit() && index - start < 4 {
            index += 1;
        }
        if index == start {
            break;
        }
        if channel == 2 {
            if index < body.len() && body[index] == b'\x1b' {
                index += 1;
            }
            break;
        }
        if index >= body.len() || body[index] != b'/' {
            break;
        }
        index += 1;
    }
    marker_len + index
}

struct Reply {
    code: u8,
    full: std::ops::Range<usize>,
    body: std::ops::Range<usize>,
}

const REPLY_MARKERS: [(&[u8], u8); 4] = [
    (b"\x1b]10;", 10),
    (b"\x1b]11;", 11),
    (b"\x9d10;", 10),
    (b"\x9d11;", 11),
];

fn find_reply(bytes: &[u8]) -> Option<Reply> {
    REPLY_MARKERS
        .iter()
        .filter_map(|(marker, code)| find_complete_reply(bytes, marker, *code))
        .min_by_key(|reply| reply.full.start)
}

fn find_complete_reply(bytes: &[u8], marker: &[u8], code: u8) -> Option<Reply> {
    let mut offset = 0;
    while let Some(relative) = find_bytes(&bytes[offset..], marker) {
        let start = offset + relative;
        let body_start = start + marker.len();
        let tail = &bytes[body_start..];
        if let Some((body_len, terminator_len)) = find_terminator(tail)
            && !REPLY_MARKERS
                .iter()
                .filter_map(|(nested, _)| find_bytes(tail, nested))
                .any(|nested| nested < body_len)
        {
            return Some(Reply {
                code,
                full: start..body_start + body_len + terminator_len,
                body: body_start..body_start + body_len,
            });
        }
        offset = start + 1;
    }
    None
}

fn find_terminator(tail: &[u8]) -> Option<(usize, usize)> {
    [b"\x07".as_slice(), b"\x1b\\".as_slice(), b"\x9c".as_slice()]
        .iter()
        .filter_map(|terminator| find_bytes(tail, terminator).map(|at| (at, terminator.len())))
        .min_by_key(|(at, _)| *at)
}

fn incomplete_reply_start(bytes: &[u8]) -> Option<usize> {
    (0..bytes.len()).find(|&start| {
        let candidate = &bytes[start..];
        let matched = REPLY_MARKERS
            .iter()
            .map(|(marker, _)| common_prefix_len(candidate, marker))
            .max()
            .unwrap_or(0);
        matched >= 3
            || (matched == 2 && candidate.len() == 2)
            || (candidate.first() == Some(&b'\x9d') && matched >= 1)
    })
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter().zip(right).take_while(|(a, b)| a == b).count()
}

fn parse_rgb(body: &[u8]) -> Option<TerminalColor> {
    let body = body.strip_prefix(b"rgb:")?;
    let mut channels = body.split(|&byte| byte == b'/');
    let r = parse_channel(channels.next()?)?;
    let g = parse_channel(channels.next()?)?;
    let b = parse_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some(TerminalColor { r, g, b })
}

fn parse_channel(hex: &[u8]) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 {
        return None;
    }
    let text = std::str::from_utf8(hex).ok()?;
    let value = u32::from_str_radix(text, 16).ok()?;
    let max = (1u32 << (hex.len() * 4)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ghostty_color_replies_and_preserves_typed_input() {
        let mut bytes =
            b"a\x1b]11;rgb:1a1a/2b2b/3c3c\x1b\\b\x1b]10;rgb:eeee/dddd/cccc\x07c".to_vec();

        let appearance = extract_terminal_replies(&mut bytes);

        assert_eq!(
            appearance,
            TerminalAppearance {
                foreground: Some(TerminalColor {
                    r: 0xee,
                    g: 0xdd,
                    b: 0xcc,
                }),
                background: Some(TerminalColor {
                    r: 0x1a,
                    g: 0x2b,
                    b: 0x3c,
                }),
            }
        );
        assert_eq!(bytes, b"abc");
    }

    #[test]
    fn extracts_c1_and_seven_bit_color_replies_with_every_osc_terminator() {
        let introducers: [&[u8]; 2] = [b"\x1b]", b"\x9d"];
        let terminators: [&[u8]; 3] = [b"\x07", b"\x1b\\", b"\x9c"];

        for introducer in introducers {
            for terminator in terminators {
                let mut bytes = b"before".to_vec();
                bytes.extend_from_slice(introducer);
                bytes.extend_from_slice(b"10;rgb:1111/2222/3333");
                bytes.extend_from_slice(terminator);
                bytes.extend_from_slice(b"after");

                let appearance = extract_terminal_replies(&mut bytes);

                assert_eq!(
                    appearance.foreground,
                    Some(TerminalColor {
                        r: 0x11,
                        g: 0x22,
                        b: 0x33,
                    }),
                    "introducer={introducer:?}, terminator={terminator:?}"
                );
                assert_eq!(bytes, b"beforeafter");
            }
        }
    }

    #[test]
    fn malformed_complete_replies_are_removed_but_never_become_black() {
        let mut bytes = b"x\x1b]11;not-rgb\x07y".to_vec();

        let appearance = extract_terminal_replies(&mut bytes);

        assert_eq!(appearance, TerminalAppearance::default());
        assert_eq!(bytes, b"xy");
    }

    #[test]
    fn variable_width_channels_scale_to_rgb24() {
        let mut bytes = b"\x1b]11;rgb:f/8/0000\x07".to_vec();

        let appearance = extract_terminal_replies(&mut bytes);

        assert_eq!(
            appearance.background,
            Some(TerminalColor {
                r: 0xff,
                g: 0x88,
                b: 0x00,
            })
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn finishing_a_probe_discards_only_the_incomplete_terminal_reply() {
        let mut bytes = b"typed\x1b]11;rgb:0101/0202".to_vec();

        finish_probe_input(&mut bytes);

        assert_eq!(bytes, b"typed");
    }

    #[test]
    fn finishing_a_probe_preserves_input_after_an_incomplete_reply() {
        let mut bytes = b"\x1b]11;rgb:0101/0202typed".to_vec();

        finish_probe_input(&mut bytes);

        assert_eq!(bytes, b"typed");
    }

    #[test]
    fn finishing_a_probe_removes_a_partial_marker_before_typed_input() {
        for (bytes, expected) in [
            (b"\x1b]1x".as_slice(), b"x".as_slice()),
            (b"\x1b]10x".as_slice(), b"x".as_slice()),
            (b"\x1b]11x".as_slice(), b"x".as_slice()),
        ] {
            let mut bytes = bytes.to_vec();
            finish_probe_input(&mut bytes);
            assert_eq!(bytes, expected);
        }
    }

    #[test]
    fn a_complete_reply_after_an_incomplete_candidate_is_still_extracted() {
        let mut bytes = b"\x1b]10;broken\x1b]11;rgb:1111/2222/3333\x07".to_vec();

        let appearance = extract_terminal_replies(&mut bytes);

        assert_eq!(
            appearance.background,
            Some(TerminalColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            })
        );
        finish_probe_input(&mut bytes);
        assert_eq!(bytes, b"broken");
    }

    #[test]
    fn multiline_probe_input_is_restored_as_bracketed_paste() {
        assert_eq!(
            prepare_probe_input(b"\x1b[200~echo one\recho two\x1b[201~".to_vec(), true),
            b"\x1b[200~echo one\recho two\x1b[201~"
        );
        assert_eq!(
            paste_bytes(b"safe\x1b[201~\rrm", true),
            b"\x1b[200~safe\rrm\x1b[201~"
        );
        assert_eq!(
            prepare_probe_input(b"\x1b[200~echo one\recho two\x1b[201~".to_vec(), false),
            b"echo one\recho two"
        );
        assert_eq!(
            prepare_probe_input(b"typed one\rtyped two".to_vec(), true),
            b"typed one\rtyped two"
        );
    }

    #[test]
    fn a_single_enter_during_the_probe_remains_a_keypress() {
        assert_eq!(prepare_probe_input(b"\r".to_vec(), true), b"\r");
    }

    #[test]
    fn detects_a_paste_split_across_probe_reads() {
        assert!(has_incomplete_bracketed_paste(b"key\x1b[200~partial"));
        assert!(!has_incomplete_bracketed_paste(
            b"key\x1b[200~complete\x1b[201~tail"
        ));
    }
}
