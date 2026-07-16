//! Regression: the cursor must stay on its own cell after a snapshot round-trip
//! even when the screen has scrolled. `snapshot_vt` dumps the *active area* only
//! (not scrollback); dumping the whole screen shifted the re-fed content so the
//! active-area cursor CUP landed a few rows too high (visible as a misplaced
//! cursor in TUIs after scrolling).

use asd_vt::{GhosttyVt, RenderSnapshot, VtBackend};

fn mark_row(snap: &RenderSnapshot) -> Option<usize> {
    snap.cells.iter().position(|row| {
        row.iter()
            .map(|c| c.grapheme.as_str())
            .collect::<String>()
            .contains("MARK")
    })
}

#[test]
fn cursor_tracks_its_cell_after_snapshot_roundtrip_with_scrollback() {
    let (cols, rows) = (20u16, 8u16);

    // Print enough lines to scroll the 8-row screen, then park the cursor on a
    // non-bottom row and stamp MARK there so cursor and cell share a row.
    let mut origin = GhosttyVt::new(cols, rows, 1000);
    for i in 1..=15 {
        origin.feed(format!("L{i:02}\r\n").as_bytes());
    }
    origin.feed(b"\x1b[4;1HMARK");

    let origin_snap = origin.render_snapshot();
    let origin_cursor = origin_snap.cursor.position.map(|(_, y)| y as usize);
    assert_eq!(
        origin_cursor,
        mark_row(&origin_snap),
        "sanity: origin cursor row equals MARK row"
    );

    // Dump and re-feed into a fresh, same-sized terminal (the attach path).
    let mut client = GhosttyVt::new(cols, rows, 1000);
    client.feed(&origin.snapshot_vt());
    let client_snap = client.render_snapshot();

    assert_eq!(
        client_snap.cursor.position.map(|(_, y)| y as usize),
        mark_row(&client_snap),
        "client cursor row must equal MARK row (cursor tracks its cell)"
    );
    // And the reconstruction must match the origin position exactly.
    assert_eq!(
        client_snap.cursor.position.map(|(_, y)| y as usize),
        origin_cursor
    );
}
