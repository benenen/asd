//! Regression: a snapshot round-trip must preserve the scrollback (with its
//! styling) so the client can scroll back through pre-attach history. The
//! snapshot is the styled whole-screen dump; seeding scrollback as plain text
//! instead lost the colours and duplicated content in redraw-heavy TUIs.

use asd_vt::{GhosttyVt, VtBackend};

#[test]
fn snapshot_roundtrip_preserves_scrollback() {
    let (cols, rows) = (20u16, 8u16);

    // 15 lines on an 8-row screen ⇒ ~7 rows of scrollback.
    let mut origin = GhosttyVt::new(cols, rows, 1000);
    for i in 1..=15 {
        origin.feed(format!("L{i:02}\r\n").as_bytes());
    }

    // Dump and re-feed into a fresh, same-sized terminal (the attach path).
    let mut client = GhosttyVt::new(cols, rows, 1000);
    client.feed(&origin.snapshot_vt());

    assert!(
        client.scrollback_rows() > 0,
        "snapshot must carry scrollback for scrolling back, got {}",
        client.scrollback_rows()
    );
}
