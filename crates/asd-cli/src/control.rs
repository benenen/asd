//! The scripting commands `send` / `peek` / `wait` (ported from boo): drive a
//! session, read its rendered screen, or block until it matches or settles —
//! all without an interactive attach, over the v4 name-addressed frames.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::bail;
use asd_proto::{ClientKind, Frame, IDLE_SETTLE_MS, MAX_FRAME_LEN};
use tokio::io::AsyncReadExt;

use crate::client;

/// Poll interval for `wait` (matches boo).
const POLL_MS: u64 = 50;
/// Process exit code on `wait` timeout (matches boo's documented code).
const EXIT_TIMEOUT: i32 = 4;

/// `asd send`: type bytes into a session's pty. The payload is `--text`
/// (literal), `--key` (named keys), or stdin; `--enter` appends a carriage
/// return. Nothing is escaped, so there is no quoting layer to fight.
pub async fn send(
    socket: &Path,
    name: String,
    text: Option<String>,
    key: Option<String>,
    enter: bool,
    stdin: bool,
) -> anyhow::Result<()> {
    // clap enforces the --text/--key/--stdin exclusivity; build the payload.
    let mut payload: Vec<u8> = if let Some(t) = text {
        t.into_bytes()
    } else if let Some(list) = key {
        let mut out = Vec::new();
        for key_name in list.split(',').filter(|k| !k.is_empty()) {
            match named_key(key_name) {
                Some(bytes) => out.extend_from_slice(&bytes),
                None => bail!("send: unknown key '{key_name}'"),
            }
        }
        out
    } else {
        let _ = stdin; // presence only forces this branch; reading is the default
        let mut buf = Vec::new();
        tokio::io::stdin().read_to_end(&mut buf).await?;
        if buf.len() > MAX_FRAME_LEN - 1024 {
            bail!("send: stdin too large (max ~4 MiB)");
        }
        buf
    };
    if enter {
        payload.push(b'\r');
    }
    if payload.is_empty() {
        bail!("send: nothing to send");
    }
    if payload.contains(&0) {
        bail!("send: cannot send NUL bytes");
    }

    let mut c = client::connect(socket, ClientKind::Cli).await?;
    c.writer
        .write_frame(&Frame::SendInput {
            name,
            bytes: payload,
        })
        .await?;
    match c.reader.read_frame().await? {
        Some(Frame::Ack) => Ok(()),
        Some(Frame::Error { code, msg }) => bail!("send failed ({code}): {msg}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// `asd peek`: print a session's rendered screen (or the full scrollback), as
/// plain text or as a JSON object.
pub async fn peek(socket: &Path, name: String, scrollback: bool, json: bool) -> anyhow::Result<()> {
    let mut c = client::connect(socket, ClientKind::Cli).await?;
    c.writer
        .write_frame(&Frame::Peek {
            name: name.clone(),
            scrollback,
        })
        .await?;
    let reply = match c.reader.read_frame().await? {
        Some(f @ Frame::PeekReply { .. }) => f,
        Some(Frame::Error { code, msg }) => bail!("peek failed ({code}): {msg}"),
        other => bail!("unexpected reply: {other:?}"),
    };
    let Frame::PeekReply {
        cols,
        rows,
        cursor_col,
        cursor_row,
        title,
        screen,
    } = reply
    else {
        unreachable!("matched PeekReply above")
    };

    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if !json {
        out.write_all(&screen)?;
        // Guarantee a trailing newline so the screen is a clean text block.
        if screen.last() != Some(&b'\n') {
            out.write_all(b"\n")?;
        }
        return Ok(());
    }

    let mut s = String::from(r#"{"session":"#);
    json_string(&name, &mut s);
    s.push_str(r#","title":"#);
    json_string(&title, &mut s);
    s.push_str(&format!(
        r#","rows":{rows},"cols":{cols},"cursor":{{"row":{cursor_row},"col":{cursor_col}}},"screen":"#
    ));
    json_string(&String::from_utf8_lossy(&screen), &mut s);
    s.push_str("}\n");
    out.write_all(s.as_bytes())?;
    Ok(())
}

/// `asd wait`: block until the session's screen contains `text`, or output has
/// been idle for [`IDLE_SETTLE_MS`], then exit 0. On timeout, exit
/// [`EXIT_TIMEOUT`]. One persistent connection is polled every [`POLL_MS`].
pub async fn wait(
    socket: &Path,
    name: String,
    text: Option<String>,
    idle: bool,
    timeout: String,
) -> anyhow::Result<()> {
    let _ = idle; // clap guarantees exactly one of --text / --idle
    let timeout_ms = parse_duration(&timeout).ok_or_else(|| {
        anyhow::anyhow!("wait: bad duration '{timeout}' (use 500ms, 2s, 1m, 4h, 1d)")
    })?;

    let mut c = client::connect(socket, ClientKind::Cli).await?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(needle) = &text {
            c.writer
                .write_frame(&Frame::Peek {
                    name: name.clone(),
                    scrollback: false,
                })
                .await?;
            match c.reader.read_frame().await? {
                Some(Frame::PeekReply { screen, .. }) => {
                    if String::from_utf8_lossy(&screen).contains(needle.as_str()) {
                        return Ok(());
                    }
                }
                Some(Frame::Error { code, msg }) => bail!("wait failed ({code}): {msg}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        } else {
            c.writer.write_frame(&Frame::ListSessions).await?;
            match c.reader.read_frame().await? {
                Some(Frame::SessionList { sessions }) => {
                    match sessions.iter().find(|s| s.name == name) {
                        Some(s) if s.idle_ms >= IDLE_SETTLE_MS => return Ok(()),
                        Some(_) => {}
                        None => bail!("wait: no session named '{name}'"),
                    }
                }
                Some(Frame::Error { code, msg }) => bail!("wait failed ({code}): {msg}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        if Instant::now() >= deadline {
            eprintln!("wait: timed out after {timeout}");
            std::process::exit(EXIT_TIMEOUT);
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

/// The byte sequence for a named key (`--key`), or `None` if unrecognized.
/// Arrow/Home/End use the legacy `ESC [ …` forms (same bytes as boo).
fn named_key(name: &str) -> Option<Vec<u8>> {
    let eqi = |a: &str| name.eq_ignore_ascii_case(a);
    let bytes: &[u8] = if eqi("enter") {
        b"\r"
    } else if eqi("tab") {
        b"\t"
    } else if eqi("escape") || eqi("esc") {
        b"\x1b"
    } else if eqi("space") {
        b" "
    } else if eqi("backspace") || eqi("bs") {
        b"\x7f"
    } else if eqi("up") {
        b"\x1b[A"
    } else if eqi("down") {
        b"\x1b[B"
    } else if eqi("right") {
        b"\x1b[C"
    } else if eqi("left") {
        b"\x1b[D"
    } else if eqi("home") {
        b"\x1b[H"
    } else if eqi("end") {
        b"\x1b[F"
    } else {
        // C-a .. C-z → control byte 0x01 .. 0x1a.
        let b = name.as_bytes();
        if b.len() == 3
            && (b[0] == b'C' || b[0] == b'c')
            && b[1] == b'-'
            && b[2].is_ascii_alphabetic()
        {
            return Some(vec![b[2].to_ascii_lowercase() - b'a' + 1]);
        }
        return None;
    };
    Some(bytes.to_vec())
}

/// Parse a duration like `500ms`, `2s`, `1m`, `4h` / `4hr`, `1d` into
/// milliseconds. Requires a unit; `None` on any malformed input.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None; // no leading integer
    }
    let value: u64 = s[..split].parse().ok()?;
    let mult: u64 = match &s[split..] {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" | "hr" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    value.checked_mul(mult)
}

/// Append `value` as a JSON-escaped string literal (with surrounding quotes)
/// to `out`. Avoids a serde_json dependency for one small object.
fn json_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_key_covers_documented_names() {
        assert_eq!(named_key("Enter").unwrap(), b"\r");
        assert_eq!(named_key("tab").unwrap(), b"\t");
        assert_eq!(named_key("ESC").unwrap(), b"\x1b");
        assert_eq!(named_key("escape").unwrap(), b"\x1b");
        assert_eq!(named_key("space").unwrap(), b" ");
        assert_eq!(named_key("Backspace").unwrap(), b"\x7f");
        assert_eq!(named_key("bs").unwrap(), b"\x7f");
        assert_eq!(named_key("Up").unwrap(), b"\x1b[A");
        assert_eq!(named_key("down").unwrap(), b"\x1b[B");
        assert_eq!(named_key("Right").unwrap(), b"\x1b[C");
        assert_eq!(named_key("left").unwrap(), b"\x1b[D");
        assert_eq!(named_key("Home").unwrap(), b"\x1b[H");
        assert_eq!(named_key("End").unwrap(), b"\x1b[F");
    }

    #[test]
    fn named_key_control_letters() {
        assert_eq!(named_key("C-a").unwrap(), vec![0x01]);
        assert_eq!(named_key("c-c").unwrap(), vec![0x03]);
        assert_eq!(named_key("C-z").unwrap(), vec![0x1a]);
    }

    #[test]
    fn named_key_rejects_unknown() {
        assert!(named_key("").is_none());
        assert!(named_key("Foo").is_none());
        assert!(named_key("C-1").is_none());
        assert!(named_key("C-ab").is_none());
        assert!(named_key("ctrl-a").is_none());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("500ms"), Some(500));
        assert_eq!(parse_duration("2s"), Some(2_000));
        assert_eq!(parse_duration("1m"), Some(60_000));
        assert_eq!(parse_duration("4h"), Some(14_400_000));
        assert_eq!(parse_duration("4hr"), Some(14_400_000));
        assert_eq!(parse_duration("1d"), Some(86_400_000));
        assert_eq!(parse_duration("30s"), Some(30_000));
    }

    #[test]
    fn parse_duration_rejects_malformed() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("s"), None); // no integer
        assert_eq!(parse_duration("10"), None); // no unit
        assert_eq!(parse_duration("10x"), None); // bad unit
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn json_string_escapes() {
        let mut s = String::new();
        json_string("a\"b\\c\nd\te", &mut s);
        assert_eq!(s, r#""a\"b\\c\nd\te""#);
        let mut s = String::new();
        json_string("\x01", &mut s);
        assert_eq!(s, "\"\\u0001\"");
    }
}
