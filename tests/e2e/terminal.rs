//! Tests that give the CLI a real PTY and assert what it paints on the way
//! out: terminal restore, synchronized-update framing, and the takeover placard.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::common::*;

static PTSNAME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A master pty and the path of its slave.
fn open_pty() -> (libc::c_int, PathBuf) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(master), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
        let path = {
            let _guard = PTSNAME_LOCK.lock().unwrap();
            let name_ptr = libc::ptsname(master);
            assert!(!name_ptr.is_null(), "ptsname failed");
            std::ffi::CStr::from_ptr(name_ptr)
                .to_string_lossy()
                .into_owned()
        };
        (master, PathBuf::from(path))
    }
}

/// Run `cmd` with `slave` as its controlling terminal, the way a shell would.
fn attach_to_pty(mut cmd: Command, slave: std::fs::File) -> Command {
    use std::os::unix::process::CommandExt;

    cmd.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        // Between fork and exec: async-signal-safe calls only. The slave is
        // already on fd 0 by now, so that is what to claim as the terminal.
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // TIOCSCTTY and the ioctl request parameter do not share a type
            // on every Unix. The conversion is load-bearing on macOS and an
            // identity on Linux, where clippy would otherwise reject it.
            #[allow(clippy::useless_conversion)]
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// A killed `asd attach` still hands the terminal back.
///
/// `attach` turns on mouse tracking (SGR 1002/1006, plus whatever the session
/// mirrors) and the alternate screen. Those are undone by a `Drop` guard, and
/// `Drop` does not run when the process is killed — so a closed tab (SIGHUP) or
/// a `kill` from elsewhere (SIGTERM) used to leave the terminal reporting every
/// mouse move as `ESC[<..M` text at the shell prompt. The same hole was closed
/// in `asd ui` before; this pins it shut for `attach`.
#[test]
fn killed_attach_restores_the_terminal() {
    let daemon = Daemon::start("attachsignal");
    assert!(
        daemon
            .cli()
            .args(["new", "term"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let (master, slave_path) = open_pty();
    let mut child = daemon.cli();
    child.args(["attach", "term"]);
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .unwrap();
    let mut child = attach_to_pty(child, slave).spawn().unwrap();

    // Read the pty in the background: `attach` writes its setup, then (with the
    // fix) the restore sequence as it dies.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break; // EIO once the last slave fd closes, i.e. the child is gone
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n as usize]);
            }
            unsafe { libc::close(master) };
        })
    };
    let saw = |needle: &[u8]| contains(&seen.lock().unwrap(), needle);

    // Wait until it has taken the terminal over (mouse tracking on).
    let deadline = std::time::Instant::now() + WAIT;
    while !saw(b"\x1b[?1002h") {
        assert!(
            std::time::Instant::now() < deadline,
            "attach never enabled mouse tracking: {:?}",
            String::from_utf8_lossy(&seen.lock().unwrap())
        );
        std::thread::sleep(TICK);
    }

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    reader.join().unwrap();

    let out = seen.lock().unwrap().clone();
    let dump = String::from_utf8_lossy(&out).into_owned();
    for off in [b"\x1b[?1002l".as_slice(), b"\x1b[?1006l", b"\x1b[?1049l"] {
        assert!(
            contains(&out, off),
            "terminal left in {:?} after SIGTERM; pty saw: {dump:?}",
            String::from_utf8_lossy(off)
        );
    }
}

/// A killed `asd ui` must close the host terminal's synchronized-update mode
/// before restoring mouse/paste/alternate-screen state. Normal frames already
/// contain `?2026l`, so inspect only bytes emitted after a quiet pre-kill
/// boundary; otherwise a completed frame could make the assertion pass while
/// the signal handler itself still omitted the close.
#[test]
fn killed_ui_closes_synchronized_update_before_restoring_terminal() {
    use std::os::unix::process::ExitStatusExt;

    let mut daemon = Daemon::start("uisignal");
    let (master, slave_path) = open_pty();
    let mut command = daemon.cli();
    command.arg("ui");
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .unwrap();
    let mut child = attach_to_pty(command, slave).spawn().unwrap();

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n as usize]);
            }
            unsafe { libc::close(master) };
        })
    };

    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let output = seen.lock().unwrap();
        if contains(&output, b"\x1b[?1002h")
            && contains(&output, b"\x1b[?2026h")
            && contains(&output, b"\x1b[?2026l")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ui never completed its first frame: {:?}",
            String::from_utf8_lossy(&output)
        );
        drop(output);
        std::thread::sleep(TICK);
    }

    // Find a quiet interval between the 1.5 s session-list polls, then record
    // the boundary immediately before SIGTERM. This makes the checked suffix
    // signal-handler output rather than an earlier normal frame.
    let mut last_len = seen.lock().unwrap().len();
    let mut stable_since = std::time::Instant::now();
    loop {
        std::thread::sleep(TICK);
        let len = seen.lock().unwrap().len();
        if len != last_len {
            last_len = len;
            stable_since = std::time::Instant::now();
        }
        if stable_since.elapsed() >= Duration::from_millis(250) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ui output never settled before SIGTERM"
        );
    }
    let kill_offset = seen.lock().unwrap().len();

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    reader.join().unwrap();

    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "killing the ui also stopped its daemon"
    );
    let out = seen.lock().unwrap().clone();
    let suffix = &out[kill_offset..];
    let restore = b"\x1b[?2026l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";
    assert!(
        suffix.starts_with(restore),
        "ui did not emit a complete ordered restore after SIGTERM; post-kill bytes: {:?}",
        String::from_utf8_lossy(suffix)
    );
    for off in [
        b"\x1b[?1002l".as_slice(),
        b"\x1b[?2004l",
        b"\x1b[?1049l",
        b"\x1b[?25h",
        b"\x1b[0m",
    ] {
        assert!(
            contains(suffix, off),
            "ui terminal restore omitted {:?}; post-kill bytes: {:?}",
            String::from_utf8_lossy(off),
            String::from_utf8_lossy(suffix)
        );
    }
}

/// Two real `asd ui` processes exercise the user-facing half of TUI takeover:
/// the displaced process stays alive, clears the terminal pane, and paints the
/// asd wordmark plus an actionable message.
#[test]
fn displaced_ui_shows_the_takeover_placard() {
    let daemon = Daemon::start("uiplacard");
    assert!(
        daemon
            .cli()
            .args(["new", "shared"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let spawn_ui = |tag: &str, session: &str| {
        use std::os::fd::AsRawFd;

        let (master, slave_path) = open_pty();
        let window = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &window) },
            0,
            "setting {tag} pty size failed"
        );
        let mut command = daemon.cli();
        command.args(["ui", session]);
        let child = attach_to_pty(command, slave).spawn().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let reader = {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                    if n <= 0 {
                        break;
                    }
                    seen.lock().unwrap().extend_from_slice(&buf[..n as usize]);
                }
                unsafe { libc::close(master) };
            })
        };
        (child, seen, reader)
    };

    let (mut first, first_output, first_reader) = spawn_ui("first", "shared");
    let attached = || {
        let out = daemon.cli().args(["list", "--json"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("\"attached_clients\":1")
    };
    let deadline = std::time::Instant::now() + WAIT;
    while !attached() {
        assert!(
            std::time::Instant::now() < deadline,
            "first ui never attached"
        );
        std::thread::sleep(TICK);
    }

    assert!(
        daemon
            .cli()
            .args(["rename", "shared", "renamed"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let (mut second, _second_output, second_reader) = spawn_ui("second", "renamed");
    let deadline = std::time::Instant::now() + WAIT;
    let saw_placard = loop {
        let output = first_output.lock().unwrap();
        if contains(&output, b"__ _ ___")
            && contains(&output, b"Session \"renamed\" is open in another asd ui")
            && contains(&output, b"Select it again to take over")
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        drop(output);
        std::thread::sleep(TICK);
    };

    unsafe {
        libc::kill(first.id() as i32, libc::SIGTERM);
        libc::kill(second.id() as i32, libc::SIGTERM);
    }
    let _ = first.wait();
    let _ = second.wait();
    first_reader.join().unwrap();
    second_reader.join().unwrap();

    assert!(
        saw_placard,
        "displaced ui output: {:?}",
        String::from_utf8_lossy(&first_output.lock().unwrap())
    );
}
