use std::io;
use std::time::Instant;

use asd_proto::TerminalAppearance;

use crate::terminal::{
    BRACKETED_PASTE_END, COLOR_QUERY, MAX_PROBE_BYTES, MAX_PROBE_PASTE_BYTES, PROBE_LATE_GRACE,
    PROBE_PASTE_TIMEOUT, PROBE_TIMEOUT, ProbeResult, extract_terminal_replies, finish_probe_input,
    has_incomplete_bracketed_paste, has_incomplete_terminal_reply,
};

const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";

pub(crate) fn probe_terminal_colors() -> io::Result<ProbeResult> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1
        || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1
    {
        return Ok(ProbeResult::default());
    }
    probe_terminal_colors_fds(libc::STDIN_FILENO, libc::STDOUT_FILENO)
}

fn probe_terminal_colors_fds(
    input_fd: libc::c_int,
    output_fd: libc::c_int,
) -> io::Result<ProbeResult> {
    probe_terminal_colors_fds_with_paste_timeout(input_fd, output_fd, PROBE_PASTE_TIMEOUT)
}

fn probe_terminal_colors_fds_with_paste_timeout(
    input_fd: libc::c_int,
    output_fd: libc::c_int,
    paste_timeout: std::time::Duration,
) -> io::Result<ProbeResult> {
    write_all(output_fd, ENABLE_BRACKETED_PASTE)?;
    let result = probe_enabled_terminal(input_fd, output_fd, paste_timeout);
    let cleanup = write_all(output_fd, DISABLE_BRACKETED_PASTE);
    match (result, cleanup) {
        (Ok(probe), Ok(())) => Ok(probe),
        (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; disabling host bracketed paste also failed: {cleanup_error}"),
        )),
    }
}

fn probe_enabled_terminal(
    input_fd: libc::c_int,
    output_fd: libc::c_int,
    paste_timeout: std::time::Duration,
) -> io::Result<ProbeResult> {
    write_all(output_fd, COLOR_QUERY)?;

    let normal_deadline = Instant::now() + PROBE_TIMEOUT;
    let late_deadline = normal_deadline + PROBE_LATE_GRACE;
    let mut partial_deadline = None;
    let mut paste_deadline = None;
    let mut input = Vec::with_capacity(MAX_PROBE_BYTES);
    let mut appearance = TerminalAppearance::default();
    loop {
        let now = Instant::now();
        let paste_incomplete = has_incomplete_bracketed_paste(&input);
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
        if input.len() >= input_limit {
            if paste_incomplete {
                drain_oversized_paste(
                    input_fd,
                    &input,
                    paste_deadline.expect("incomplete paste has a deadline"),
                )?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "terminal paste exceeds protocol input limit",
                ));
            }
            break;
        }
        if has_incomplete_terminal_reply(&input) && partial_deadline.is_none() {
            partial_deadline = Some(now + PROBE_TIMEOUT);
        }
        let deadline = if paste_incomplete {
            paste_deadline.expect("incomplete paste has a deadline")
        } else {
            partial_deadline.map_or(late_deadline, |partial| partial.max(late_deadline))
        };
        if !poll_readable(input_fd, deadline)? {
            if paste_incomplete {
                discard_pending_input(input_fd).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "terminal paste timed out and discarding pending input failed: {error}"
                        ),
                    )
                })?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal paste did not provide an end marker",
                ));
            }
            break;
        }

        let mut chunk = [0u8; 128];
        let want = chunk.len().min(input_limit - input.len());
        let read = read_retry(input_fd, &mut chunk[..want])?;
        if read == 0 {
            break;
        }
        input.extend_from_slice(&chunk[..read]);
        appearance = merge(appearance, extract_terminal_replies(&mut input));
        if appearance.foreground.is_some()
            && appearance.background.is_some()
            && !has_incomplete_terminal_reply(&input)
            && !has_incomplete_bracketed_paste(&input)
        {
            break;
        }
    }
    finish_probe_input(&mut input);

    Ok(ProbeResult { appearance, input })
}

fn merge(current: TerminalAppearance, found: TerminalAppearance) -> TerminalAppearance {
    TerminalAppearance {
        foreground: current.foreground.or(found.foreground),
        background: current.background.or(found.background),
    }
}

fn poll_readable(fd: libc::c_int, deadline: Instant) -> io::Result<bool> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready > 0 {
            if pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("terminal color probe poll failed"));
            }
            return Ok(pollfd.revents & libc::POLLIN != 0);
        }
        if ready == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn drain_oversized_paste(fd: libc::c_int, captured: &[u8], deadline: Instant) -> io::Result<()> {
    let keep = BRACKETED_PASTE_END.len().saturating_sub(1);
    let mut pending = captured[captured.len().saturating_sub(keep)..].to_vec();
    let mut chunk = [0u8; 8192];
    loop {
        if !poll_readable(fd, deadline)? {
            discard_pending_input(fd).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "terminal paste timed out and discarding pending input failed: {error}"
                    ),
                )
            })?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal paste did not provide an end marker",
            ));
        }
        let read = read_retry(fd, &mut chunk)?;
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

fn discard_pending_input(fd: libc::c_int) -> io::Result<()> {
    loop {
        if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_retry(fd: libc::c_int, bytes: &mut [u8]) -> io::Result<usize> {
    loop {
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn write_all(fd: libc::c_int, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "terminal color probe write returned zero",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::thread;

    use super::*;
    use asd_proto::TerminalColor;

    #[test]
    fn accepts_a_complete_reply_arriving_after_the_normal_deadline() {
        let (master, slave) = open_raw_pty();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let responder = thread::spawn(move || {
            wait_for_query(master.as_raw_fd());
            thread::sleep(PROBE_TIMEOUT + std::time::Duration::from_millis(50));
            write_all(
                master.as_raw_fd(),
                b"\x1b]10;rgb:eeee/dddd/cccc\x07\x1b]11;rgb:1111/2222/3333\x07",
            )
            .unwrap();
            done_rx.recv().unwrap();
        });

        let result = probe_terminal_colors_fds(slave.as_raw_fd(), slave.as_raw_fd()).unwrap();
        done_tx.send(()).unwrap();
        responder.join().unwrap();

        assert_eq!(
            result.appearance.foreground,
            Some(TerminalColor {
                r: 0xee,
                g: 0xdd,
                b: 0xcc
            })
        );
        assert_eq!(
            result.appearance.background,
            Some(TerminalColor {
                r: 0x11,
                g: 0x22,
                b: 0x33
            })
        );
        assert!(result.input.is_empty());
    }

    #[test]
    fn captures_a_long_slow_paste_without_closing_it_early() {
        let (master, slave) = open_raw_pty();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let content = vec![b'x'; MAX_PROBE_BYTES + 256];
        let expected = content.clone();
        let responder = thread::spawn(move || {
            wait_for_query(master.as_raw_fd());
            let mut first =
                b"\x1b]10;rgb:eeee/dddd/cccc\x07\x1b]11;rgb:1111/2222/3333\x07\x1b[200~".to_vec();
            first.extend_from_slice(&content[..MAX_PROBE_BYTES]);
            write_all(master.as_raw_fd(), &first).unwrap();
            thread::sleep(PROBE_TIMEOUT + std::time::Duration::from_millis(50));
            let mut rest = content[MAX_PROBE_BYTES..].to_vec();
            rest.extend_from_slice(b"\x1b[201~");
            write_all(master.as_raw_fd(), &rest).unwrap();
            done_rx.recv().unwrap();
        });

        let result = probe_terminal_colors_fds(slave.as_raw_fd(), slave.as_raw_fd()).unwrap();
        done_tx.send(()).unwrap();
        responder.join().unwrap();

        assert_eq!(
            crate::terminal::prepare_probe_input(result.input, true),
            crate::terminal::paste_bytes(&expected, true)
        );
    }

    #[test]
    fn unterminated_paste_times_out_instead_of_hanging_raw_mode() {
        let (master, slave) = open_raw_pty();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let responder = thread::spawn(move || {
            wait_for_query(master.as_raw_fd());
            write_all(
                master.as_raw_fd(),
                b"\x1b]10;rgb:eeee/dddd/cccc\x07\x1b]11;rgb:1111/2222/3333\x07\x1b[200~partial",
            )
            .unwrap();
            done_rx.recv().unwrap();
        });

        let error = probe_terminal_colors_fds_with_paste_timeout(
            slave.as_raw_fd(),
            slave.as_raw_fd(),
            std::time::Duration::from_millis(50),
        )
        .unwrap_err();
        done_tx.send(()).unwrap();
        responder.join().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    fn wait_for_query(fd: libc::c_int) {
        let mut query = Vec::new();
        let mut chunk = [0u8; 128];
        while !query
            .windows(COLOR_QUERY.len())
            .any(|bytes| bytes == COLOR_QUERY)
        {
            let read = read_retry(fd, &mut chunk).unwrap();
            assert!(read > 0);
            query.extend_from_slice(&chunk[..read]);
        }
    }

    fn open_raw_pty() -> (OwnedFd, OwnedFd) {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut termios) },
            0
        );
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) },
            0
        );
        (master, slave)
    }
}
