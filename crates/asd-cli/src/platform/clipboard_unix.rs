//! Native Unix clipboard tools; no GUI framework is linked into the CLI.

use super::clipboard::{MAX_IMAGE_BYTES, run_helper};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Desktop {
    Mac,
    Wayland,
    X11,
}

fn select_desktop(mac: bool, wayland: bool, x11: bool) -> anyhow::Result<Desktop> {
    if mac {
        Ok(Desktop::Mac)
    } else if wayland {
        Ok(Desktop::Wayland)
    } else if x11 {
        Ok(Desktop::X11)
    } else {
        anyhow::bail!(
            "no local desktop clipboard: use an image file, or run in a Wayland/X11 desktop session"
        )
    }
}

fn desktop() -> anyhow::Result<Desktop> {
    select_desktop(
        cfg!(target_os = "macos"),
        std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()),
        std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()),
    )
}

pub(crate) fn read_clipboard_image() -> anyhow::Result<Vec<u8>> {
    let bytes = match desktop()? {
        Desktop::Mac => run_helper("pngpaste", &["-"], None, MAX_IMAGE_BYTES),
        Desktop::Wayland => run_helper(
            "wl-paste",
            &["--no-newline", "--type", "image/png"],
            None,
            MAX_IMAGE_BYTES,
        ),
        Desktop::X11 => run_helper(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
            None,
            MAX_IMAGE_BYTES,
        ),
    }?;
    if bytes.is_empty() {
        anyhow::bail!("clipboard contains no PNG image; copy an image or use an image file");
    }
    Ok(bytes)
}

pub(crate) fn copy_clipboard_text(text: &str) -> anyhow::Result<()> {
    match desktop()? {
        Desktop::Mac => run_helper("pbcopy", &[], Some(text.as_bytes()), 0),
        Desktop::Wayland => run_helper(
            "wl-copy",
            &["--type", "text/plain;charset=utf-8"],
            Some(text.as_bytes()),
            0,
        ),
        Desktop::X11 => run_helper(
            "xclip",
            &["-selection", "clipboard", "-t", "UTF8_STRING", "-i"],
            Some(text.as_bytes()),
            0,
        ),
    }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_selection_prefers_native_wayland_and_rejects_headless() {
        assert_eq!(select_desktop(true, true, true).unwrap(), Desktop::Mac);
        assert_eq!(select_desktop(false, true, true).unwrap(), Desktop::Wayland);
        assert_eq!(select_desktop(false, false, true).unwrap(), Desktop::X11);
        assert!(select_desktop(false, false, false).is_err());
    }
}
