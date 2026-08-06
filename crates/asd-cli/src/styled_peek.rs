//! Style-aware `asd peek --json --styles` output.
//!
//! The stable PeekReply intentionally contains only plain text. Reusing the
//! existing Attach/Snapshot path exposes the daemon's authoritative VT state
//! without a protocol change, so a new CLI can work with an already-running
//! daemon. A zero-size entry reaches `apply_resize`'s no-op branch; sessions
//! that already have a real attached viewer are refused.

use std::io::Write as _;

use anyhow::bail;
use asd_proto::{Frame, Scrollback};
use asd_vt::{GhosttyVt, RenderSnapshot, VtBackend};

use crate::client::Client;

/// A half-open terminal-cell range carrying the faint SGR attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FaintRange {
    row: u16,
    start_col: u16,
    end_col: u16,
}

/// Attach just long enough to obtain the style-preserving VT snapshot, then
/// detach before parsing or printing it. Dropping the connection would also
/// detach, but the explicit frame keeps a successful command's lifecycle
/// independent of socket teardown timing.
pub(crate) async fn print_json(
    client: &mut Client,
    name: &str,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    require_unattached(client, name, cols, rows).await?;
    client
        .writer
        .write_frame(&Frame::Attach {
            name: name.to_string(),
            // The current daemon ignores a negotiated zero dimension in
            // apply_resize. This obtains Snapshot without sending SIGWINCH.
            cols: 0,
            rows: 0,
        })
        .await?;
    let snapshot_vt = match client.reader.read_frame().await? {
        Some(Frame::Snapshot { vt }) => vt,
        Some(Frame::Error { code, msg }) => {
            return Err(crate::exit::daemon("peek", code, &msg));
        }
        other => bail!("expected Snapshot for styled peek, got {other:?}"),
    };
    client.writer.write_frame(&Frame::Detach).await?;
    client
        .writer
        .write_frame(&Frame::Peek {
            name: name.to_string(),
            scrollback: Scrollback::None,
        })
        .await?;

    let mut vt = GhosttyVt::new(cols.max(1), rows.max(1), 0);
    vt.feed(&snapshot_vt);
    let anchor = loop {
        match client.reader.read_frame().await? {
            // Output queued before Detach belongs to the same styled view and
            // must be replayed before comparing it with the ordered Peek.
            Some(Frame::Output { bytes }) => vt.feed(&bytes),
            Some(frame @ Frame::PeekReply { .. }) => break frame,
            Some(Frame::Error { code, msg }) => {
                return Err(crate::exit::daemon("peek", code, &msg));
            }
            other => bail!("unexpected reply while anchoring styled peek: {other:?}"),
        }
    };
    let output = json_from_model(name, &mut vt, anchor)?;
    std::io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

async fn require_unattached(
    client: &mut Client,
    name: &str,
    expected_cols: u16,
    expected_rows: u16,
) -> anyhow::Result<()> {
    client.writer.write_frame(&Frame::ListSessions).await?;
    let sessions = match client.reader.read_frame().await? {
        Some(Frame::SessionList { sessions }) => sessions,
        Some(Frame::Error { code, msg }) => {
            return Err(crate::exit::daemon("peek", code, &msg));
        }
        other => bail!("unexpected reply while checking styled peek: {other:?}"),
    };
    let Some(info) = sessions.into_iter().find(|info| info.name == name) else {
        return Err(crate::exit::daemon(
            "peek",
            asd_proto::code::NO_SUCH_SESSION,
            &format!("no such session '{name}'"),
        ));
    };
    if info.cols != expected_cols || info.rows != expected_rows {
        bail!("styled peek changed size before it could be captured; try again");
    }
    if info.attached_clients != 0 {
        bail!("styled peek refuses a session with an attached viewer; detach it and try again");
    }
    Ok(())
}

#[cfg(test)]
fn json_from_vt(name: &str, cols: u16, rows: u16, vt_bytes: &[u8]) -> String {
    let mut vt = GhosttyVt::new(cols.max(1), rows.max(1), 0);
    vt.feed(vt_bytes);
    json_from_rendered_model(name, &mut vt)
}

fn json_from_model(name: &str, vt: &mut GhosttyVt, anchor: Frame) -> anyhow::Result<String> {
    let Frame::PeekReply {
        cols,
        rows,
        cursor_col,
        cursor_row,
        title,
        screen,
    } = anchor
    else {
        unreachable!("styled peek anchor is a PeekReply")
    };

    let _ = vt.take_pty_responses();
    let snapshot = vt.render_snapshot();
    let local_screen = screen_text(vt, snapshot.rows);
    let local_cursor = snapshot.cursor.position.unwrap_or((0, 0));
    let anchored_screen = String::from_utf8_lossy(&screen);
    if snapshot.cols != cols
        || snapshot.rows != rows
        || local_cursor != (cursor_col, cursor_row)
        || local_screen != anchored_screen
    {
        bail!("styled peek changed while it was being captured; try again");
    }

    Ok(json_from_snapshot(name, &title, &local_screen, &snapshot))
}

#[cfg(test)]
fn json_from_rendered_model(name: &str, vt: &mut GhosttyVt) -> String {
    let _ = vt.take_pty_responses();
    let title = vt.title();
    let snapshot = vt.render_snapshot();
    let screen = screen_text(vt, snapshot.rows);
    json_from_snapshot(name, &title, &screen, &snapshot)
}

fn json_from_snapshot(name: &str, title: &str, screen: &str, snapshot: &RenderSnapshot) -> String {
    let ranges = faint_ranges(snapshot);
    let (cursor_col, cursor_row) = snapshot.cursor.position.unwrap_or((0, 0));

    let mut out = String::from("{\"session\":");
    crate::control::json_string(name, &mut out);
    out.push_str(",\"title\":");
    // Keep the public JSON shape byte-for-byte compatible with plain peek;
    // `faint_ranges` is the only additional field.
    crate::control::json_string(title, &mut out);
    out.push_str(&format!(
        ",\"rows\":{},\"cols\":{},\"cursor\":{{\"row\":{},\"col\":{}}},\"screen\":",
        snapshot.rows, snapshot.cols, cursor_row, cursor_col
    ));
    crate::control::json_string(screen, &mut out);
    out.push_str(",\"faint_ranges\":[");
    for (index, range) in ranges.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            r#"{{"row":{},"start_col":{},"end_col":{}}}"#,
            range.row, range.start_col, range.end_col
        ));
    }
    out.push_str("]}\n");
    out
}

fn screen_text(vt: &mut GhosttyVt, rows: u16) -> String {
    let history = vt.scrollback_rows() as u32;
    let lines = vt.fetch_history(history, u32::from(rows));
    let mut end = lines.len();
    while end > 0 && lines[end - 1].iter().all(|byte| *byte == b' ') {
        end -= 1;
    }
    lines[..end]
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn faint_ranges(snapshot: &RenderSnapshot) -> Vec<FaintRange> {
    let mut ranges = Vec::new();
    for (row, cells) in snapshot.cells.iter().enumerate() {
        let mut start = None;
        for (col, cell) in cells.iter().enumerate() {
            match (start, cell.flags.faint) {
                (None, true) => start = Some(col),
                (Some(first), false) => {
                    ranges.push(FaintRange {
                        row: row as u16,
                        start_col: first as u16,
                        end_col: col as u16,
                    });
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            ranges.push(FaintRange {
                row: row as u16,
                start_col: first as u16,
                end_col: cells.len() as u16,
            });
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use asd_proto::Frame;
    use asd_vt::{GhosttyVt, VtBackend};

    use super::{json_from_model, json_from_vt};

    #[test]
    fn json_marks_dynamic_faint_text_without_a_placeholder_list() {
        let vt = b"\x1b[1;3H\x1b[2mDynamic hint\x1b[0m\x1b[1;3H";

        let json = json_from_vt("s", 20, 2, vt);

        assert!(json.contains(r#""screen":"  Dynamic hint""#), "{json}");
        assert!(
            json.contains(r#""faint_ranges":[{"row":0,"start_col":2,"end_col":14}]"#),
            "{json}"
        );
        assert!(json.contains(r#""cursor":{"row":0,"col":2}"#), "{json}");
    }

    #[test]
    fn json_does_not_mark_identical_normal_text_as_faint() {
        let vt = b"\x1b[1;3HDynamic hint\x1b[1;3H";

        let json = json_from_vt("s", 20, 2, vt);

        assert!(json.contains(r#""screen":"  Dynamic hint""#), "{json}");
        assert!(json.contains(r#""faint_ranges":[]"#), "{json}");
    }

    #[test]
    fn faint_ranges_use_terminal_cells_for_wide_text() {
        let vt = "\x1b[1;3H\x1b[2m中文\x1b[0m\x1b[1;3H".as_bytes();

        let json = json_from_vt("s", 20, 2, vt);

        assert!(json.contains(r#""screen":"  中文""#), "{json}");
        assert!(
            json.contains(r#""faint_ranges":[{"row":0,"start_col":2,"end_col":6}]"#),
            "{json}"
        );
    }

    #[test]
    fn an_inconsistent_post_detach_peek_is_rejected() {
        let mut vt = GhosttyVt::new(20, 2, 0);
        vt.feed(b"\x1b[1;3H\x1b[2mDynamic hint\x1b[0m\x1b[1;3H");
        let anchor = Frame::PeekReply {
            cols: 20,
            rows: 2,
            cursor_col: 2,
            cursor_row: 0,
            title: String::new(),
            screen: b"  changed underneath".to_vec(),
        };

        let error = json_from_model("s", &mut vt, anchor).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("changed while it was being captured")
        );
    }

    #[test]
    fn post_detach_title_is_metadata_not_part_of_vt_consistency() {
        let mut vt = GhosttyVt::new(20, 2, 0);
        vt.feed(b"\x1b[1;3H\x1b[2mDynamic hint\x1b[0m\x1b[1;3H");
        let anchor = Frame::PeekReply {
            cols: 20,
            rows: 2,
            cursor_col: 2,
            cursor_row: 0,
            title: "Codex session title".to_string(),
            screen: b"  Dynamic hint".to_vec(),
        };

        let json = json_from_model("s", &mut vt, anchor).unwrap();

        assert!(json.contains(r#""title":"Codex session title""#), "{json}");
    }
}
