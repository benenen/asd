//! Prepare a small image as a remote-shell command, without executing or sending it.
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Args;
use sha2::{Digest, Sha256};

/// Keep the encoded command comfortably below the protocol's 4 MiB frame cap.
const MAX_IMAGE_BYTES: usize = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = asd_proto::MAX_FRAME_LEN - 1024;

#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("image_source").required(true).args(["file", "clipboard"])))]
pub(crate) struct ImagePasteArgs {
    /// Local PNG, JPEG, GIF, or WebP file, at most 1 MiB.
    file: Option<std::path::PathBuf>,
    /// Read a screenshot from this computer's clipboard instead of a file.
    #[arg(long)]
    clipboard: bool,
    /// Copy the generated command to this computer's clipboard instead of stdout.
    #[arg(long)]
    copy: bool,
    /// Existing directory in the destination shell; never resolved locally.
    #[arg(long, default_value = ".")]
    directory: String,
}

pub(crate) fn run(args: ImagePasteArgs) -> anyhow::Result<()> {
    let bytes = match args.file {
        Some(path) => read_file(&path)?,
        None => crate::platform::read_clipboard_image()?,
    };
    let command = prepare(&bytes, &args.directory)?;
    if args.copy {
        crate::platform::copy_clipboard_text(&command)?;
        eprintln!(
            "Image decode command copied. Paste at a remote POSIX shell prompt, then press Enter. Python 3 is required; no command has been executed."
        );
    } else {
        // A trailing newline could submit the heredoc terminator when pasted.
        std::io::stdout().lock().write_all(command.as_bytes())?;
        eprintln!(
            "Prepared image decode command. Paste at a remote POSIX shell prompt, then press Enter. Python 3 is required; no command has been executed."
        );
    }
    Ok(())
}

fn read_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    if !std::fs::metadata(path)
        .context("cannot inspect local image file")?
        .is_file()
    {
        bail!("image-paste requires a regular image file");
    }
    let file = std::fs::File::open(path).context("cannot open local image file")?;
    if !file.metadata()?.is_file() {
        bail!("image-paste requires a regular image file");
    }
    let mut bytes = Vec::new();
    file.take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Signature detection chooses the destination suffix, without decoding pixels.
fn suffix(bytes: &[u8]) -> anyhow::Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(".png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Ok(".jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok(".gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Ok(".webp")
    } else {
        bail!("unsupported image signature; use a PNG, JPEG, GIF, or WebP file")
    }
}

fn prepare(bytes: &[u8], directory: &str) -> anyhow::Result<String> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        bail!("image must contain 1 byte to 1 MiB; resize large screenshots first");
    }
    if directory.is_empty()
        || directory.len() > 4096
        || directory.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        bail!(
            "destination directory must be nonempty, at most 4096 bytes, and contain no control characters"
        );
    }
    let extension = suffix(bytes)?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    let encoded = STANDARD.encode(bytes);
    let literals = encoded
        .as_bytes()
        .chunks(76)
        .map(|line| {
            format!(
                "    \"{}\"\n",
                std::str::from_utf8(line).expect("base64 is ASCII")
            )
        })
        .collect::<String>();
    // JSON string syntax is a safe Python string literal for this validated text.
    let directory = serde_json::to_string(directory)?;
    let command = format!(
        "python3 - <<'ASD_IMAGE_PAYLOAD'\n\
import base64, hashlib, os, tempfile\n\
data = base64.b64decode(\n{literals}    , validate=True)\n\
if len(data) != {size} or hashlib.sha256(data).hexdigest() != '{digest}':\n    raise ValueError('image payload is incomplete or corrupted')\n\
fd, path = tempfile.mkstemp(prefix='asd-image-', suffix='{extension}', dir={directory})\n\
try:\n    with os.fdopen(fd, 'wb') as image:\n        image.write(data)\n        image.flush()\n        os.fsync(image.fileno())\n\
except BaseException:\n    os.unlink(path)\n    raise\n\
print(os.path.abspath(path))\n\
ASD_IMAGE_PAYLOAD",
        size = bytes.len()
    );
    if command.len() > MAX_COMMAND_BYTES {
        bail!("encoded command exceeds the terminal input limit");
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn source_selection_is_explicit_and_exclusive() {
        assert!(crate::Args::try_parse_from(["asd", "image-paste"]).is_err());
        assert!(
            crate::Args::try_parse_from(["asd", "image-paste", "a.png", "--clipboard"]).is_err()
        );
        assert!(
            crate::Args::try_parse_from(["asd", "image-paste", "--clipboard", "--copy"]).is_ok()
        );
        assert!(
            crate::Args::try_parse_from(["asd", "image-paste", "a.png", "--directory", "/remote"])
                .is_ok()
        );
    }

    #[test]
    fn generated_command_is_one_quoted_heredoc_without_submission_newline() {
        let command = prepare(b"\x89PNG\r\n\x1a\npayload", ".").unwrap();
        assert!(command.starts_with("python3 - <<'ASD_IMAGE_PAYLOAD'\n"));
        assert!(command.ends_with("\nASD_IMAGE_PAYLOAD"));
        assert!(!command.ends_with('\n'));
        assert!(command.contains("hashlib.sha256(data)"));
        assert!(command.contains("tempfile.mkstemp"));
    }

    #[test]
    fn payload_size_boundary_is_enforced_after_bounded_read() {
        let mut data = vec![0; MAX_IMAGE_BYTES];
        data[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let command = prepare(&data, ".").unwrap();
        assert!(command.len() < MAX_COMMAND_BYTES);
        assert!(command.lines().all(|line| line.len() < 1024));
        data.push(0);
        assert!(prepare(&data, ".").is_err());
        assert!(prepare(&[], ".").is_err());
    }

    #[test]
    fn rejects_unknown_format_and_control_characters_in_destination() {
        assert!(prepare(b"plain text", ".").is_err());
        for directory in ["", "a\nb", "\x1b[0m", "\u{202e}"] {
            assert!(prepare(b"GIF89apayload", directory).is_err());
        }
        assert!(prepare(b"GIF89apayload", "a'\"$(touch marker)`x`\\directory").is_ok());
    }

    #[test]
    fn detects_supported_image_suffixes() {
        assert_eq!(suffix(b"\xff\xd8\xffrest").unwrap(), ".jpg");
        assert_eq!(suffix(b"GIF87arest").unwrap(), ".gif");
        assert_eq!(suffix(b"RIFF1234WEBPrest").unwrap(), ".webp");
        assert!(suffix(b"RIFF1234WAVErest").is_err());
    }
}
