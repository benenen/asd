use std::io;
use std::ptr;
use std::time::{Duration, Instant};

use asd_proto::TerminalAppearance;
use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, GetConsoleMode, GetStdHandle,
    INPUT_RECORD, KEY_EVENT, PeekConsoleInputW, ReadConsoleInputW, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use crate::terminal::{
    BRACKETED_PASTE_END, COLOR_QUERY, MAX_PROBE_BYTES, MAX_PROBE_PASTE_BYTES, PROBE_LATE_GRACE,
    PROBE_PASTE_TIMEOUT, PROBE_TIMEOUT, ProbeResult, extract_terminal_replies, finish_probe_input,
    has_incomplete_bracketed_paste, has_incomplete_terminal_reply,
};

const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";

pub(crate) fn probe_terminal_colors() -> io::Result<ProbeResult> {
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let Some(mut modes) = ConsoleModes::enable(input, output)? else {
        return Ok(ProbeResult::default());
    };

    let result = probe_with_bracketed_paste(input, output);
    let restore = modes.restore();
    combine_results(result, restore, "restoring Windows console modes")
}

fn probe_with_bracketed_paste(input: HANDLE, output: HANDLE) -> io::Result<ProbeResult> {
    write_all(output, ENABLE_BRACKETED_PASTE)?;
    let result = probe_enabled_terminal(input, output, PROBE_PASTE_TIMEOUT);
    let cleanup = write_all(output, DISABLE_BRACKETED_PASTE);
    combine_results(result, cleanup, "disabling host bracketed paste")
}

fn probe_enabled_terminal(
    input: HANDLE,
    output: HANDLE,
    paste_timeout: Duration,
) -> io::Result<ProbeResult> {
    write_all(output, COLOR_QUERY)?;

    let normal_deadline = Instant::now() + PROBE_TIMEOUT;
    let late_deadline = normal_deadline + PROBE_LATE_GRACE;
    let mut partial_deadline = None;
    let mut paste_deadline = None;
    let mut input_bytes = Vec::with_capacity(MAX_PROBE_BYTES);
    let mut appearance = TerminalAppearance::default();
    loop {
        let now = Instant::now();
        let paste_incomplete = has_incomplete_bracketed_paste(&input_bytes);
        if paste_incomplete && paste_deadline.is_none() {
            paste_deadline = Some(now + paste_timeout);
        } else if !paste_incomplete {
            paste_deadline = None;
        }
        let input_limit = if paste_incomplete {
            MAX_PROBE_PASTE_BYTES
        } else {
            MAX_PROBE_BYTES
        };
        if input_bytes.len() >= input_limit {
            if paste_incomplete {
                drain_oversized_paste(
                    input,
                    &input_bytes,
                    paste_deadline.expect("incomplete paste has a deadline"),
                )?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "terminal paste exceeds protocol input limit",
                ));
            }
            break;
        }
        if has_incomplete_terminal_reply(&input_bytes) && partial_deadline.is_none() {
            partial_deadline = Some(now + PROBE_TIMEOUT);
        }
        let deadline = if paste_incomplete {
            paste_deadline.expect("incomplete paste has a deadline")
        } else {
            partial_deadline.map_or(late_deadline, |partial| partial.max(late_deadline))
        };
        if !wait_readable(input, deadline)? {
            if paste_incomplete {
                discard_pending_input(input)?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal paste did not provide an end marker",
                ));
            }
            break;
        }

        let mut chunk = [0u8; 128];
        let want = chunk.len().min(input_limit - input_bytes.len());
        let read = read_once(input, &mut chunk[..want])?;
        if read == 0 {
            break;
        }
        input_bytes.extend_from_slice(&chunk[..read]);
        appearance = merge(appearance, extract_terminal_replies(&mut input_bytes));
        if appearance.foreground.is_some()
            && appearance.background.is_some()
            && !has_incomplete_terminal_reply(&input_bytes)
            && !has_incomplete_bracketed_paste(&input_bytes)
        {
            break;
        }
    }
    finish_probe_input(&mut input_bytes);

    Ok(ProbeResult {
        appearance,
        input: input_bytes,
    })
}

fn merge(current: TerminalAppearance, found: TerminalAppearance) -> TerminalAppearance {
    TerminalAppearance {
        foreground: current.foreground.or(found.foreground),
        background: current.background.or(found.background),
    }
}

fn wait_readable(handle: HANDLE, deadline: Instant) -> io::Result<bool> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        let timeout_ms = remaining.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
        if !wait(handle, timeout_ms)? {
            return Ok(false);
        }
        if discard_non_character_events(handle)? {
            return Ok(true);
        }
    }
}

fn wait(handle: HANDLE, timeout_ms: u32) -> io::Result<bool> {
    match unsafe { WaitForSingleObject(handle, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        status => Err(io::Error::other(format!(
            "unexpected Windows console wait status {status}"
        ))),
    }
}

fn drain_oversized_paste(handle: HANDLE, captured: &[u8], deadline: Instant) -> io::Result<()> {
    let keep = BRACKETED_PASTE_END.len().saturating_sub(1);
    let mut pending = captured[captured.len().saturating_sub(keep)..].to_vec();
    let mut chunk = [0u8; 8192];
    loop {
        if !wait_readable(handle, deadline)? {
            discard_pending_input(handle)?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal paste did not provide an end marker",
            ));
        }
        let read = read_once(handle, &mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal closed during oversized paste",
            ));
        }
        pending.extend_from_slice(&chunk[..read]);
        if pending
            .windows(BRACKETED_PASTE_END.len())
            .any(|bytes| bytes == BRACKETED_PASTE_END)
        {
            return Ok(());
        }
        if pending.len() > keep {
            pending.drain(..pending.len() - keep);
        }
    }
}

fn discard_pending_input(handle: HANDLE) -> io::Result<()> {
    let mut records = [INPUT_RECORD::default(); 64];
    while wait(handle, 0)? {
        if read_input_records(handle, &mut records)? == 0 {
            break;
        }
    }
    Ok(())
}

/// Remove console records that can wake the input handle but can never make
/// `ReadFile` return bytes. Without this step an old mouse/window/KeyUp record
/// could satisfy `WaitForSingleObject`, then leave the bounded probe blocked in
/// a character read. Returns true when the first remaining record is a
/// KeyDown that the console can translate to VT input.
fn discard_non_character_events(handle: HANDLE) -> io::Result<bool> {
    loop {
        let mut record = INPUT_RECORD::default();
        let mut count = 0u32;
        if unsafe { PeekConsoleInputW(handle, &mut record, 1, &mut count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Ok(false);
        }
        if record.EventType == KEY_EVENT as u16 {
            let key = unsafe { record.Event.KeyEvent };
            if key.bKeyDown != 0 && !is_modifier_key(key.wVirtualKeyCode) {
                return Ok(true);
            }
        }
        let mut discarded = INPUT_RECORD::default();
        read_input_records(handle, std::slice::from_mut(&mut discarded))?;
    }
}

fn is_modifier_key(virtual_key: u16) -> bool {
    // Standalone modifier/toggle events have no character representation.
    // Their state is already carried on the following real KeyDown record.
    matches!(virtual_key, 0x10..=0x14 | 0x90 | 0x91)
}

fn read_input_records(handle: HANDLE, records: &mut [INPUT_RECORD]) -> io::Result<usize> {
    let mut read = 0u32;
    let ok = unsafe {
        ReadConsoleInputW(
            handle,
            records.as_mut_ptr(),
            records.len().min(u32::MAX as usize) as u32,
            &mut read,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(read as usize)
    }
}

fn read_once(handle: HANDLE, buffer: &mut [u8]) -> io::Result<usize> {
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr(),
            buffer.len().min(u32::MAX as usize) as u32,
            &mut read,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(read as usize)
    }
}

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "Windows console accepted zero probe bytes",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn combine_results<T>(
    result: io::Result<T>,
    cleanup: io::Result<()>,
    cleanup_action: &str,
) -> io::Result<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; {cleanup_action} also failed: {cleanup_error}"),
        )),
    }
}

struct ConsoleModes {
    input: HANDLE,
    output: HANDLE,
    input_mode: u32,
    output_mode: u32,
    active: bool,
}

impl ConsoleModes {
    fn enable(input: HANDLE, output: HANDLE) -> io::Result<Option<Self>> {
        if !valid_handle(input) || !valid_handle(output) {
            return Ok(None);
        }
        let (Some(input_mode), Some(output_mode)) = (console_mode(input), console_mode(output))
        else {
            return Ok(None);
        };
        let input_probe_mode = (input_mode | ENABLE_VIRTUAL_TERMINAL_INPUT)
            & !(ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT);
        let output_probe_mode =
            output_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;

        set_console_mode(input, input_probe_mode)?;
        if let Err(error) = set_console_mode(output, output_probe_mode) {
            return combine_results(
                Err(error),
                set_console_mode(input, input_mode),
                "restoring Windows console input mode",
            )
            .map(|()| None);
        }
        Ok(Some(Self {
            input,
            output,
            input_mode,
            output_mode,
            active: true,
        }))
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let input = set_console_mode(self.input, self.input_mode);
        let output = set_console_mode(self.output, self.output_mode);
        combine_results(input, output, "restoring Windows console output mode")
    }
}

impl Drop for ConsoleModes {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn valid_handle(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

fn console_mode(handle: HANDLE) -> Option<u32> {
    let mut mode = 0u32;
    (unsafe { GetConsoleMode(handle, &mut mode) } != 0).then_some(mode)
}

fn set_console_mode(handle: HANDLE, mode: u32) -> io::Result<()> {
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
