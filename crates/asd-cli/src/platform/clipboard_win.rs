//! Windows clipboard access through static STA PowerShell scripts.

use anyhow::Context;
use base64::Engine as _;

use super::clipboard::{MAX_IMAGE_BYTES, run_helper};

const READ_IMAGE: &str = r#"$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$image = [System.Windows.Forms.Clipboard]::GetImage()
if ($null -eq $image) { throw 'Clipboard contains no image' }
$stream = New-Object System.IO.MemoryStream
try {
    $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    if ($stream.Length -gt 1048576) { throw 'Clipboard image exceeds 1 MiB' }
    [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()))
} finally {
    $stream.Dispose()
    $image.Dispose()
}"#;

const COPY_TEXT: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false)
Set-Clipboard -Value ([Console]::In.ReadToEnd())"#;

fn powershell(script: &str, input: Option<&[u8]>, limit: usize) -> anyhow::Result<Vec<u8>> {
    run_helper(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-STA", "-Command", script],
        input,
        limit,
    )
}

pub(crate) fn read_clipboard_image() -> anyhow::Result<Vec<u8>> {
    let encoded = powershell(READ_IMAGE, None, MAX_IMAGE_BYTES.div_ceil(3) * 4)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Windows clipboard helper returned invalid image data")?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("clipboard image must contain between 1 byte and 1 MiB");
    }
    Ok(bytes)
}

pub(crate) fn copy_clipboard_text(text: &str) -> anyhow::Result<()> {
    powershell(COPY_TEXT, Some(text.as_bytes()), 0)?;
    Ok(())
}
