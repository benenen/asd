//! session = PTY + child process + headless Terminal + scrollback (spec §5).
//!
//! Threading model: one std thread per session owns its Terminal exclusively
//! (`GhosttyVt` is `!Send`, so it cannot leave the thread — enforced at
//! compile time); pty reads, Input frames, and Resize all enter that thread
//! via a channel. The network side (tokio) holds only a [`SessionHandle`].

use std::io::Write;
#[cfg(unix)]
use std::os::unix::io::RawFd;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, mpsc};

use asd_proto::{AgentState, Frame, IDLE_SETTLE_MS, TerminalAppearance, TerminalColor, code};

use crate::detect::Detector;
use asd_vt::{ColorQueryFilter, GhosttyVt, Rgb, VtBackend};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tracing::{debug, info, warn};

use crate::registry::Registry;

/// Per-client Output send-queue cap (spec §5, M0-era flow control):
/// a full queue means the client is dead → disconnect it; the session is
/// unaffected.
pub const OUTPUT_QUEUE_CAP: usize = 4 * 1024 * 1024;
/// Quiet interval between scripted text and its requested Enter keypress.
const SCRIPT_ENTER_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Queue element from connection tasks → the socket write loop.
#[derive(Debug)]
pub enum ConnItem {
    Frame(Frame),
    /// Forced disconnect (emitted by the sink on flow-control overflow or
    /// session death).
    Close,
}

pub type OutTx = tokio::sync::mpsc::UnboundedSender<ConnItem>;

/// The session thread's outlet for delivering frames to one attached client.
///
/// Byte quota: only data-plane frame (Snapshot/Output) payloads count; the
/// connection write loop returns the same quota as each frame is written
/// out. On overflow it sends `Close` to the connection and reports the
/// client dead.
#[derive(Debug, Clone)]
pub struct ClientSink {
    pub id: u64,
    tx: OutTx,
    queued: Arc<AtomicUsize>,
}

impl ClientSink {
    pub fn new(id: u64, tx: OutTx, queued: Arc<AtomicUsize>) -> Self {
        Self { id, tx, queued }
    }

    /// Deliver one frame; `false` means the client is dead (overflow or the
    /// connection already closed) and the caller should remove it from the
    /// broadcast list.
    pub fn send(&self, frame: Frame) -> bool {
        let sz = data_frame_size(&frame);
        let queued = self.queued.load(Ordering::Relaxed);
        // Queue non-empty and enqueueing again would exceed the cap → the
        // client consumes too slowly; declare it dead and disconnect
        if queued > 0 && queued + sz > OUTPUT_QUEUE_CAP {
            warn!(
                client = self.id,
                queued, "output queue overflow, dropping client"
            );
            let _ = self.tx.send(ConnItem::Close);
            return false;
        }
        self.queued.fetch_add(sz, Ordering::Relaxed);
        self.tx.send(ConnItem::Frame(frame)).is_ok()
    }
}

/// Quota usage of data-plane frames; control-plane frames take no quota.
pub fn data_frame_size(frame: &Frame) -> usize {
    match frame {
        Frame::Output { bytes } | Frame::Input { bytes } => bytes.len(),
        Frame::Snapshot { vt } => vt.len(),
        _ => 0,
    }
}

/// Whether an attached client is a shared viewer or the one exclusive
/// ratatui view. This policy stays behind the daemon's session seam; wire
/// clients only declare their connection kind during the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachClass {
    Shared,
    ExclusiveTui,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TuiOwner {
    client_id: u64,
    view_id: u64,
}

fn attach_view_rename(
    class: AttachClass,
    requested_name: &str,
    canonical_name: &str,
    view_id: u64,
) -> Option<Frame> {
    (class == AttachClass::ExclusiveTui && requested_name != canonical_name).then(|| {
        Frame::ViewRenamed {
            old_name: requested_name.to_string(),
            new_name: canonical_name.to_string(),
            view_id,
        }
    })
}

/// Messages sent to the session thread.
pub enum SessionMsg {
    /// Raw output fed in by the pty read thread.
    PtyOutput(Vec<u8>),
    /// The session's terminal condition, with what reported it (for the log).
    /// The pty read hitting EOF/error is one; where the pty outlives its child
    /// — a ConPTY does — [`crate::platform::watch_child_exit`] reports the
    /// child's exit as the other.
    Ended(&'static str),
    /// Client input (already-encoded bytes), written only while that client is
    /// still attached. Revocation takes effect at this membership check.
    Input {
        client_id: u64,
        bytes: Vec<u8>,
    },
    /// Attach-free scripted input. The session thread performs the optional
    /// Enter itself so no other client's input can interleave with the pair.
    ScriptInput {
        bytes: Vec<u8>,
        enter: bool,
        completed: tokio::sync::oneshot::Sender<std::io::Result<()>>,
    },
    /// Resize policy v1: "last Attach/Resize wins" (spec §5).
    Resize {
        /// Which viewer changed size; the pty is sized from all of them.
        client_id: u64,
        cols: u16,
        rows: u16,
    },
    /// Attach: reply with Snapshot first, then join the broadcast list; the
    /// ordering is guaranteed by the single channel.
    Attach {
        sink: ClientSink,
        class: AttachClass,
        requested_name: String,
        view_id: u64,
        cols: u16,
        rows: u16,
        appearance: TerminalAppearance,
    },
    /// Keep the exclusive TUI owner's client-side view tag aligned with the
    /// canonical session name, including renames initiated by another client.
    ViewRenamed {
        old_name: String,
        new_name: String,
    },
    Detach {
        client_id: u64,
    },
    /// Fetch a scrollback window for one client (v1). Replies with a
    /// `History` frame on that client's sink.
    FetchHistory {
        sink: ClientSink,
        start: u32,
        count: u32,
    },
    /// Send a fresh `Snapshot` of the live screen to one client (v1). Used to
    /// resync after the client leaves its local scrollback view.
    Refresh {
        sink: ClientSink,
    },
    /// Render a plain-text dump of the screen (v4, `asd peek`); replies with a
    /// `PeekReply` on `sink`. `scrollback` says how much history to prepend.
    Peek {
        sink: ClientSink,
        scrollback: asd_proto::Scrollback,
    },
    /// Join the output stream without attaching (v9, `asd follow`): replies
    /// with the current `FollowStatus`, then gets every pty batch. Followers
    /// are kept apart from `clients` on purpose — they take no part in size
    /// negotiation and are not counted as attached, so watching a session
    /// cannot change what the people attached to it see.
    Follow {
        sink: ClientSink,
    },
    /// Leave the output stream (v9). Dropping the connection has the same
    /// effect; this is for a client that carries on afterwards.
    Unfollow {
        client_id: u64,
    },
    /// Detailed single-session dump (v6, `asd inspect`); replies with an
    /// `InspectReply` on `sink`. `info` is the metadata gathered on the network
    /// thread; the session thread adds the live VT state.
    Inspect {
        sink: ClientSink,
        info: asd_proto::SessionInfo,
    },
    /// Kill the session: SIGHUP the child, then SIGKILL if it is still alive
    /// after 2s.
    Kill,
}

/// Server-side cap on rows returned per `FetchHistory` (keeps a `History`
/// frame well under the 4 MiB cap; the client paginates as it scrolls).
pub const MAX_HISTORY_ROWS_PER_FETCH: u32 = 2000;

/// The session handle held by the network side (metadata + message inlet).
#[derive(Clone)]
pub struct SessionHandle {
    pub name: String,
    /// The command this session runs (the `Create` cmd, or the default shell).
    pub command: String,
    /// The command the session was asked for, when it was given one: what
    /// `--cmd` said. `None` for a plain shell. Kept apart from `command`
    /// because that one is a display string that falls back to the shell, while
    /// this is the thing the persisted list restores — and a restored session
    /// runs a shell while still remembering the command staged in it.
    pub spawn_command: Option<String>,
    pub created_ms: u64,
    pub tx: mpsc::Sender<SessionMsg>,
    pub meta: Arc<SessionMeta>,
}

#[derive(Debug)]
pub struct SessionMeta {
    pub cols: AtomicU16,
    pub rows: AtomicU16,
    pub attached_clients: AtomicU32,
    pub child_pid: AtomicU32,
    pub alive: AtomicBool,
    /// Raw fd of the pty master, for reading the foreground process group
    /// (`tcgetpgrp`). `-1` when unavailable. Read-only from the network side.
    pub pty_master_fd: AtomicI32,
    /// The terminal title (OSC 0/2), exported by the session thread after each
    /// output batch; the network side reads it for `SessionInfo`.
    pub title: Mutex<String>,
    /// What the program on the screen is doing, as the detection rules read it.
    /// Written by the session thread — the only owner of the terminal model —
    /// and read by the network side for `SessionInfo`.
    pub state: Mutex<AgentState>,
    /// Unix-epoch ms of the session's last pty output, stamped by the session
    /// thread each output batch. The network side derives `idle_ms` from it for
    /// `SessionInfo` (drives `asd wait --idle`). Initialized to `created_ms`.
    pub last_output_ms: AtomicU64,
    /// The session's current name — the single source of truth once a `Rename`
    /// can change it. The registry updates this under its lock when it moves the
    /// map key; `info()` reports it and the session thread removes by it at exit,
    /// so a rename stays consistent even as the session ends.
    pub name: Mutex<String>,
}

impl SessionHandle {
    pub fn info(&self) -> asd_proto::SessionInfo {
        // Report the live foreground command (what's actually running in the
        // terminal now), falling back to the spawn command when it can't be
        // resolved (session gone, or no /proc — e.g. non-Linux).
        let fd = self.meta.pty_master_fd.load(Ordering::Relaxed);
        let command = foreground_command(fd).unwrap_or_else(|| self.command.clone());
        let title = self
            .meta
            .title
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        let idle_ms = now_ms().saturating_sub(self.meta.last_output_ms.load(Ordering::Relaxed));
        // The current name lives in `meta` so a rename is reflected here.
        let name = self
            .meta
            .name
            .lock()
            .map(|n| n.clone())
            .unwrap_or_else(|_| self.name.clone());
        asd_proto::SessionInfo {
            name,
            command,
            title,
            pid: self.meta.child_pid.load(Ordering::Relaxed),
            created_ms: self.created_ms,
            idle_ms,
            // "Running" = recently producing output; for an agent this tracks
            // working vs done (see `SessionInfo.running`).
            running: idle_ms < asd_proto::IDLE_SETTLE_MS,
            state: self.meta.state.lock().map(|s| *s).unwrap_or_default(),
            attached_clients: self.meta.attached_clients.load(Ordering::Relaxed),
            cols: self.meta.cols.load(Ordering::Relaxed),
            rows: self.meta.rows.load(Ordering::Relaxed),
        }
    }
}

/// Current Unix-epoch time in milliseconds (0 if the clock is before the
/// epoch, which never happens in practice).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The terminal's foreground command: the process group in the foreground of
/// the pty (`tcgetpgrp` on the master), resolved to a command via `/proc`
/// (Linux) or libproc's `proc_pidpath` (macOS). `None` when there is no
/// foreground group or the platform has no cheap way to resolve it.
#[cfg(unix)]
fn foreground_command(master_fd: RawFd) -> Option<String> {
    if master_fd < 0 {
        return None;
    }
    // SAFETY: a plain read syscall on the fd; the master stays open for the
    // session's lifetime, and a stale fd just yields an error → None.
    let pgrp = unsafe { libc::tcgetpgrp(master_fd) };
    if pgrp <= 0 {
        return None;
    }
    proc_command(pgrp)
}

#[cfg(windows)]
fn foreground_command(_master_fd: i32) -> Option<String> {
    // TODO: read foreground process from ConPTY process list
    None
}

/// Strip the leading `-` of a login shell's argv[0] (`-bash` → `bash`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn strip_login_dash(s: &str) -> &str {
    s.strip_prefix('-').unwrap_or(s)
}

/// A `--cmd` session's foreground is our non-interactive `sh -c <c>` wrapper
/// (no job control, so sh stays the group leader). Show the command it runs,
/// not the wrapper. Interactive foreground jobs get their own process group and
/// never look like this.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unwrap_shell_c(cmd: String) -> String {
    for prefix in ["sh -c ", "bash -c ", "dash -c ", "zsh -c "] {
        if let Some(rest) = cmd.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    cmd
}

/// Format process `pid`'s command from `/proc/<pid>/cmdline` — argv[0]
/// basenamed (and de-`-`ed for login shells), remaining args kept — falling
/// back to `/proc/<pid>/comm`.
#[cfg(target_os = "linux")]
fn proc_command(pid: libc::pid_t) -> Option<String> {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let mut argv = raw.split(|&b| b == 0).filter(|s| !s.is_empty());
        if let Some(arg0) = argv.next() {
            let arg0 = String::from_utf8_lossy(arg0);
            let base = arg0.rsplit('/').next().unwrap_or(&arg0);
            let mut out = strip_login_dash(base).to_string();
            for arg in argv {
                out.push(' ');
                out.push_str(&String::from_utf8_lossy(arg));
            }
            return Some(unwrap_shell_c(out));
        }
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(c) if !c.trim().is_empty() => Some(c.trim().to_string()),
        _ => None,
    }
}

/// macOS has no `/proc`. Get the full argv via `sysctl(KERN_PROCARGS2)` for
/// parity with Linux, falling back to libproc's `proc_pidpath` (executable
/// basename) if the argv read fails.
#[cfg(target_os = "macos")]
fn proc_command(pid: libc::pid_t) -> Option<String> {
    procargs2(pid).or_else(|| proc_pidpath_basename(pid))
}

/// Full command line from `sysctl(KERN_PROCARGS2)`: argv[0] basenamed, args
/// kept, our `sh -c` wrapper stripped — same shape as the Linux `/proc` path.
#[cfg(target_os = "macos")]
fn procargs2(pid: libc::pid_t) -> Option<String> {
    // Size the buffer from KERN_ARGMAX (the args+env blob can't exceed it).
    let mut argmax: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: mib has `namelen` entries; oldp/oldlenp point at `argmax`/`len`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut argmax as *mut libc::c_int).cast::<libc::c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }

    let mut buf = vec![0u8; argmax as usize];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: mib has 3 entries; oldp is `buf` (len bytes), oldlenp is `len`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    parse_procargs2(&buf)
}

/// Parse a `KERN_PROCARGS2` blob: `[argc:i32][exec_path\0][\0…][argv0\0]…`.
/// Pure byte parsing (no syscalls), so it is built under `test` on Linux too,
/// which is how the macOS argv parser keeps unit tests on a Linux dev box.
///
/// The test arm is pinned to Linux rather than left open: the body needs
/// `libc::c_int` and the `strip_login_dash`/`unwrap_shell_c` helpers, none of
/// which exist on Windows, so a bare `test` compiled this into the Windows test
/// build and broke it.
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
fn parse_procargs2(buf: &[u8]) -> Option<String> {
    let int_sz = std::mem::size_of::<libc::c_int>();
    let argc = i32::from_ne_bytes(buf.get(..int_sz)?.try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    let mut i = int_sz;
    // Skip the exec path, then the run of NUL padding before argv[0].
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    // Read `argc` NUL-terminated argv strings.
    let mut argv: Vec<String> = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        argv.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        i += 1; // step over the NUL
    }
    let arg0 = argv.first()?;
    let base = strip_login_dash(arg0.rsplit('/').next().unwrap_or(arg0));
    let mut out = base.to_string();
    for arg in &argv[1..] {
        out.push(' ');
        out.push_str(arg);
    }
    Some(unwrap_shell_c(out))
}

/// The foreground process's executable basename via libproc `proc_pidpath` —
/// the macOS fallback when the argv read is unavailable.
#[cfg(target_os = "macos")]
fn proc_pidpath_basename(pid: libc::pid_t) -> Option<String> {
    const MAX: usize = 4096; // PROC_PIDPATHINFO_MAXSIZE (4 * MAXPATHLEN)
    let mut buf = [0u8; MAX];
    // SAFETY: `buf` is valid for `MAX` bytes; `proc_pidpath` writes at most that
    // many and returns the byte length written (<= 0 on failure).
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast::<libc::c_void>(), MAX as u32) };
    if n <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..n as usize]).ok()?;
    let base = strip_login_dash(path.rsplit('/').next().unwrap_or(path));
    (!base.is_empty()).then(|| base.to_string())
}

/// No cheap foreground-command source on other unix targets. Not defined for
/// Windows at all: `libc::pid_t` does not exist there, and nothing calls this —
/// the Windows `foreground_command` answers `None` without ever resolving a pid.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn proc_command(_pid: libc::pid_t) -> Option<String> {
    None
}

/// What a session needs from the daemon that owns it. One value rather than a
/// growing tail of parameters through `spawn_session` into the session thread.
#[derive(Clone)]
pub struct SessionContext {
    /// The listener this daemon serves, exported to the child as `$ASD_SOCKET`.
    pub socket: std::path::PathBuf,
    /// Agent-detection rules, loaded once and shared by every session.
    pub detector: Arc<Detector>,
}

/// The environment a session's child gets on top of the daemon's own: what it
/// is running in, and which daemon owns it.
///
/// `socket` is the listener this daemon actually serves, not
/// [`asd_proto::paths::socket_path`]'s answer. A daemon started with `--socket`
/// would otherwise leave its children resolving the default path, so an `asd`
/// command run *inside* a session would address a different daemon than the one
/// hosting it.
fn set_session_env(builder: &mut CommandBuilder, name: &str, socket: &std::path::Path) {
    builder.env("TERM", "xterm-256color");
    // Which session a process runs inside (tmux's $TMUX idea): render clients
    // check it to refuse attaching the session that hosts them — attaching
    // yourself is a render feedback loop that floods the pty.
    builder.env("ASD_SESSION", name);
    builder.env("ASD_SOCKET", socket);
}

/// Create the pty, start the child process, and launch the session thread
/// and pty read thread.
#[allow(clippy::too_many_arguments)]
pub fn spawn_session(
    name: String,
    cmd: Option<String>,
    cwd: Option<std::path::PathBuf>,
    cols: u16,
    rows: u16,
    scrollback: usize,
    context: SessionContext,
    registry: Arc<Mutex<Registry>>,
) -> anyhow::Result<SessionHandle> {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // Display string for `SessionInfo.command`: the user command as given, or
    // the resolved default shell when none was.
    let command = cmd
        .clone()
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()));

    let mut builder = match &cmd {
        // The user command is parsed via sh -c, supporting arguments/pipes
        Some(c) => {
            let mut b = CommandBuilder::new("/bin/sh");
            b.args(["-c", c]);
            b
        }
        None => CommandBuilder::new_default_prog(), // $SHELL
    };
    set_session_env(&mut builder, &name, &context.socket);
    // Working directory: the requested one (a restart workspace restore) when it
    // still exists, else the process default ($HOME). A stale/missing dir must
    // not fail the spawn — fall back rather than error.
    let start_dir = cwd
        .filter(|d| d.is_dir())
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
    if let Some(dir) = start_dir {
        builder.cwd(dir);
    }

    let child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);
    let child_pid = child.process_id().unwrap_or(0);

    let master = pair.master;
    // Raw fd for foreground-process lookups; the master owns it and stays open
    // for the session's lifetime (this is a borrow, not a dup). `-1` where the
    // platform has no fd to borrow — see `platform::pty_master_fd`.
    let master_fd = crate::platform::pty_master_fd(master.as_ref());
    let pty_writer = master.take_writer()?;
    let pty_reader = master.try_clone_reader()?;

    let created_ms = now_ms();

    let (tx, rx) = mpsc::channel::<SessionMsg>();
    let meta = Arc::new(SessionMeta {
        cols: AtomicU16::new(cols),
        rows: AtomicU16::new(rows),
        attached_clients: AtomicU32::new(0),
        child_pid: AtomicU32::new(child_pid),
        alive: AtomicBool::new(true),
        pty_master_fd: AtomicI32::new(master_fd),
        title: Mutex::new(String::new()),
        state: Mutex::new(AgentState::default()),
        last_output_ms: AtomicU64::new(created_ms),
        name: Mutex::new(name.clone()),
    });

    // pty read thread: blocking reads → feed into the session thread
    {
        let tx = tx.clone();
        let name = name.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{name}"))
            .spawn(move || {
                let mut reader = pty_reader;
                let mut buf = [0u8; 8192];
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(SessionMsg::Ended("pty eof"));
                            break;
                        }
                        Ok(n) => {
                            if tx.send(SessionMsg::PtyOutput(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            })?;
    }

    // Child-exit watch: where the pty outlives its child, this is the only
    // ending the session gets (a no-op where the pty reports EOF by itself).
    crate::platform::watch_child_exit(child_pid, &name, tx.clone());

    // Session thread: exclusive owner of the Terminal and the pty master
    {
        let name = name.clone();
        let meta = Arc::clone(&meta);
        std::thread::Builder::new()
            .name(format!("session-{name}"))
            .spawn(move || {
                session_thread(
                    name, rx, master, pty_writer, child, cols, rows, scrollback, context, meta,
                    registry,
                );
            })?;
    }

    Ok(SessionHandle {
        name,
        command,
        spawn_command: cmd,
        created_ms,
        tx,
        meta,
    })
}

#[allow(clippy::too_many_arguments)]
fn session_thread(
    name: String,
    rx: mpsc::Receiver<SessionMsg>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut pty_writer: Box<dyn Write + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    cols: u16,
    rows: u16,
    scrollback: usize,
    context: SessionContext,
    meta: Arc<SessionMeta>,
    registry: Arc<Mutex<Registry>>,
) {
    let mut vt = GhosttyVt::new(cols, rows, scrollback);
    let mut client_output_filter = ColorQueryFilter::default();
    let mut terminal_appearance = TerminalAppearance::default();
    let mut clients: Vec<ClientSink> = Vec::new();
    // At most one ratatui client owns the interactive TUI view. Shared CLI/GUI
    // attachments remain in `clients` and are never displaced by this slot.
    let mut tui_owner: Option<TuiOwner> = None;
    // Each attached client's window size; the pty follows the smallest.
    let mut client_sizes: std::collections::HashMap<u64, (u16, u16)> = Default::default();
    // `asd follow` subscribers. Deliberately not `clients`: they get Output but
    // no Snapshot, and they neither resize the pty nor count as attached.
    let mut followers: Vec<ClientSink> = Vec::new();
    // Whether the followers have already been told this quiet spell began.
    let mut idle_announced = false;
    // Detection throttle: when the last one ran, and whether output has arrived
    // since that still needs one.
    let mut last_detect_ms = 0u64;
    let mut detect_pending = false;
    info!(session = %name, pid = meta.child_pid.load(Ordering::Relaxed), "session started");

    loop {
        // Two things can become true with no message arriving: a quiet spell a
        // follower is waiting to hear about, and a detection the throttle
        // deferred. Each contributes a deadline, and the wait takes whichever
        // comes first; with neither pending, block exactly as before, since
        // nothing can change until the next message.
        let until_idle = (!followers.is_empty() && !idle_announced).then(|| {
            let idle_ms = now_ms().saturating_sub(meta.last_output_ms.load(Ordering::Relaxed));
            IDLE_SETTLE_MS.saturating_sub(idle_ms).max(1)
        });
        let until_detect = detect_pending.then(|| {
            DETECT_INTERVAL_MS
                .saturating_sub(now_ms().saturating_sub(last_detect_ms))
                .max(1)
        });
        let deadline = match (until_idle, until_detect) {
            (Some(idle), Some(detect)) => Some(idle.min(detect)),
            (idle, detect) => idle.or(detect),
        };
        let msg = match deadline {
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            },
            Some(ms) => match rx.recv_timeout(std::time::Duration::from_millis(ms)) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if detect_pending {
                        update_agent_state(&context.detector, &mut vt, &meta);
                        last_detect_ms = now_ms();
                        detect_pending = false;
                    }
                    if !followers.is_empty() && !idle_announced {
                        idle_announced = notify_followers(&mut followers, &meta);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        match msg {
            SessionMsg::PtyOutput(bytes) => {
                // Clients render their own terminal models, some of which also
                // answer OSC queries. Keep theme queries daemon-only so one
                // shared PTY receives exactly one response.
                let client_bytes = client_output_filter.push(&bytes);
                vt.feed(&bytes);
                // Stamp the output time so the network side can report idle_ms
                // (drives `asd wait --idle`).
                meta.last_output_ms.store(now_ms(), Ordering::Relaxed);
                // Export the terminal title for the session list (cheap; only
                // written when it actually changed).
                let title = vt.title();
                if let Ok(mut shared) = meta.title.lock()
                    && *shared != title
                {
                    *shared = title;
                }
                // Read what the new screen says the program is doing, at most
                // every DETECT_INTERVAL_MS; anything sooner is owed until the
                // loop's deadline comes round.
                if now_ms().saturating_sub(last_detect_ms) >= DETECT_INTERVAL_MS {
                    update_agent_state(&context.detector, &mut vt, &meta);
                    last_detect_ms = now_ms();
                    detect_pending = false;
                } else {
                    detect_pending = true;
                }
                // The terminal's replies to DA/DSR-style queries must be
                // written back to the pty, otherwise capability probes in
                // vim/htop hang
                if let Err(error) = flush_pty_responses(&mut vt, &mut pty_writer) {
                    warn!(
                        session = %name,
                        error = %error,
                        "writing terminal query response to pty failed; ending session"
                    );
                    request_child_shutdown(&meta);
                    break;
                }
                let output = (!client_bytes.is_empty()).then_some(Frame::Output {
                    bytes: client_bytes,
                });
                if let Some(output) = &output {
                    let dropped = broadcast(&mut clients, &meta, output.clone());
                    if dropped > 0 {
                        tui_owner = tui_owner.filter(|owner| {
                            clients.iter().any(|client| client.id == owner.client_id)
                        });
                        resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                    }
                }
                // Followers see the same bytes, then where that leaves the
                // session. Sending the pair from here — inside the one thread
                // that serializes everything about this session — is what lets
                // a follower trust that the status describes the output it just
                // read, rather than whatever a separate poll happened to catch.
                if !followers.is_empty() {
                    if let Some(output) = output {
                        followers.retain(|f| f.send(output.clone()));
                    }
                    notify_followers(&mut followers, &meta);
                }
                idle_announced = false;
            }
            SessionMsg::Input { client_id, bytes } => {
                if !clients.iter().any(|client| client.id == client_id) {
                    continue;
                }
                if pty_writer
                    .write_all(&bytes)
                    .and_then(|()| pty_writer.flush())
                    .is_err()
                {
                    debug!(session = %name, "pty write failed (child likely exited)");
                }
            }
            SessionMsg::ScriptInput {
                bytes,
                enter,
                completed,
            } => {
                let result = write_script_input(&mut pty_writer, &bytes, enter, std::thread::sleep);
                let _ = completed.send(result);
            }
            SessionMsg::Resize {
                client_id,
                cols,
                rows,
            } => {
                if !clients.iter().any(|client| client.id == client_id) {
                    continue;
                }
                client_sizes.insert(client_id, (cols, rows));
                resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
            }
            SessionMsg::Attach {
                sink,
                class,
                requested_name,
                view_id,
                cols,
                rows,
                appearance,
            } => {
                let canonical_name = meta
                    .name
                    .lock()
                    .map(|name| name.clone())
                    .unwrap_or_else(|_| name.clone());
                if class == AttachClass::ExclusiveTui
                    && let Some(previous) = tui_owner.take()
                    && previous.client_id != sink.id
                    && let Some(index) = clients
                        .iter()
                        .position(|client| client.id == previous.client_id)
                {
                    let displaced = clients.remove(index);
                    client_sizes.remove(&previous.client_id);
                    displaced.send(Frame::ViewRevoked {
                        name: canonical_name.clone(),
                        view_id: previous.view_id,
                    });
                    meta.attached_clients
                        .store(clients.len() as u32, Ordering::Relaxed);
                }
                if let Some(rename) =
                    attach_view_rename(class, &requested_name, &canonical_name, view_id)
                    && !sink.send(rename)
                {
                    resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                    continue;
                }
                let adopted = merge_terminal_appearance(terminal_appearance, appearance);
                let new_foreground = terminal_appearance
                    .foreground
                    .is_none()
                    .then_some(adopted.foreground)
                    .flatten()
                    .map(vt_rgb);
                let new_background = terminal_appearance
                    .background
                    .is_none()
                    .then_some(adopted.background)
                    .flatten()
                    .map(vt_rgb);
                terminal_appearance = adopted;
                vt.set_default_colors(new_foreground, new_background);
                // A query can predate the first attach. Release its now-known
                // reply before taking the Snapshot so the child can continue.
                if let Err(error) = flush_pty_responses(&mut vt, &mut pty_writer) {
                    warn!(
                        session = %name,
                        error = %error,
                        "writing terminal query response to pty failed; ending session"
                    );
                    sink.send(Frame::Error {
                        code: code::SESSION_EXITED,
                        msg: format!("session '{name}' pty write failed"),
                    });
                    request_child_shutdown(&meta);
                    break;
                }
                // The newcomer joins the size negotiation before its snapshot
                // is taken, so the dump it gets already describes the size
                // everyone ends up at.
                let new_id = sink.id;
                client_sizes.insert(new_id, (cols, rows));
                let mut with_new: Vec<ClientSink> = clients.clone();
                with_new.push(sink.clone());
                resize_to_clients(&*master, &mut vt, &meta, &with_new, &mut client_sizes);
                let snapshot = vt.snapshot_vt();
                // The Snapshot is enqueued before any subsequent Output (the
                // single channel preserves order)
                if sink.send(Frame::Snapshot { vt: snapshot }) {
                    clients.push(sink);
                    if class == AttachClass::ExclusiveTui {
                        tui_owner = Some(TuiOwner {
                            client_id: new_id,
                            view_id,
                        });
                    }
                    meta.attached_clients
                        .store(clients.len() as u32, Ordering::Relaxed);
                    info!(session = %name, clients = clients.len(), "client attached");
                } else {
                    client_sizes.remove(&new_id);
                    resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                }
            }
            SessionMsg::ViewRenamed { old_name, new_name } => {
                if let Some(owner) = tui_owner
                    && let Some(client) = clients.iter().find(|client| client.id == owner.client_id)
                    && !client.send(Frame::ViewRenamed {
                        old_name,
                        new_name,
                        view_id: owner.view_id,
                    })
                {
                    remove_client_membership(
                        owner.client_id,
                        &mut clients,
                        &mut tui_owner,
                        &mut client_sizes,
                    );
                    meta.attached_clients
                        .store(clients.len() as u32, Ordering::Relaxed);
                    resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                }
            }
            SessionMsg::Detach { client_id } => {
                remove_client_membership(
                    client_id,
                    &mut clients,
                    &mut tui_owner,
                    &mut client_sizes,
                );
                meta.attached_clients
                    .store(clients.len() as u32, Ordering::Relaxed);
                resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                debug!(session = %name, client = client_id, "client detached");
            }
            SessionMsg::Follow { sink } => {
                // Answer with the state as it stands before streaming anything:
                // a follower that arrives after the session has already gone
                // quiet has to learn that now, not wait for output that is
                // never coming.
                let (status, running) = follow_status(&meta);
                if sink.send(status) {
                    debug!(session = %name, client = sink.id, "follower joined");
                    followers.push(sink);
                }
                idle_announced = !running;
            }
            SessionMsg::Unfollow { client_id } => {
                followers.retain(|f| f.id != client_id);
                debug!(session = %name, client = client_id, "follower left");
            }
            SessionMsg::FetchHistory { sink, start, count } => {
                if !clients.iter().any(|client| client.id == sink.id) {
                    continue;
                }
                let count = count.min(MAX_HISTORY_ROWS_PER_FETCH);
                let total_rows = vt.history_len() as u32;
                let rows = vt.fetch_history(start, count);
                // Reply on the requesting client's own sink. History is not a
                // data-plane frame, so it does not consume the flow-control
                // quota; the window is bounded by MAX_HISTORY_ROWS_PER_FETCH.
                let client_id = sink.id;
                if !sink.send(Frame::History {
                    total_rows,
                    start,
                    rows,
                }) && remove_client_membership(
                    client_id,
                    &mut clients,
                    &mut tui_owner,
                    &mut client_sizes,
                ) {
                    meta.attached_clients
                        .store(clients.len() as u32, Ordering::Relaxed);
                    resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                }
            }
            SessionMsg::Refresh { sink } => {
                if !clients.iter().any(|client| client.id == sink.id) {
                    continue;
                }
                let snapshot = vt.snapshot_vt();
                let client_id = sink.id;
                if !sink.send(Frame::Snapshot { vt: snapshot })
                    && remove_client_membership(
                        client_id,
                        &mut clients,
                        &mut tui_owner,
                        &mut client_sizes,
                    )
                {
                    meta.attached_clients
                        .store(clients.len() as u32, Ordering::Relaxed);
                    resize_to_clients(&*master, &mut vt, &meta, &clients, &mut client_sizes);
                }
            }
            SessionMsg::Peek { sink, scrollback } => {
                sink.send(render_peek(&mut vt, scrollback));
            }
            SessionMsg::Inspect { sink, info } => {
                let snap = vt.render_snapshot();
                let (cursor_col, cursor_row) = snap.cursor.position.unwrap_or((0, 0));
                sink.send(Frame::InspectReply {
                    info,
                    child_pid: meta.child_pid.load(Ordering::Relaxed),
                    alt_screen: vt.is_alt_screen(),
                    scrollback_rows: vt.scrollback_rows() as u32,
                    mouse_tracking: vt.is_mouse_tracking(),
                    mouse_modes: vt.mouse_modes(),
                    cursor_col,
                    cursor_row,
                    cursor_visible: snap.cursor.visible,
                });
            }
            SessionMsg::Kill => {
                info!(session = %name, "kill requested");
                request_child_shutdown(&meta);
            }
            SessionMsg::Ended(reason) => {
                // The filter's lookahead can be sitting on the child's last
                // bytes, and after the break nothing else will push them out.
                let tail = client_output_filter.finish();
                if !tail.is_empty() {
                    let output = Frame::Output { bytes: tail };
                    broadcast(&mut clients, &meta, output.clone());
                    followers.retain(|f| f.send(output.clone()));
                }
                info!(session = %name, reason, "session ending");
                break;
            }
        }
    }

    // Endpoint: reap the child, deregister, broadcast the exit, and
    // disconnect all clients
    let _ = child.wait();
    meta.alive.store(false, Ordering::Relaxed);
    meta.child_pid.store(0, Ordering::Relaxed);
    // Remove by the current name — a rename may have changed the map key since
    // spawn (the canonical name lives in `meta`).
    let current = meta
        .name
        .lock()
        .map(|n| n.clone())
        .unwrap_or_else(|_| name.clone());
    registry.lock().unwrap().remove(&current);
    for c in clients.drain(..) {
        c.send(Frame::Error {
            code: code::SESSION_EXITED,
            msg: format!("session '{name}' exited"),
        });
        // The sink is dropped by the drain; the connection side sees the
        // channel close after writing out the tail of its queue
    }
    // Followers get both endings, because they stop on different ones: the
    // default `follow` returns on `running == false`, while `--forever`
    // ignores that by definition and needs to be told the session is gone.
    // Neither can be left out — a follower is not in `clients`, so this is its
    // only notice, and without it `follow` would sit there until its timeout
    // for a session that no longer exists.
    for f in followers.drain(..) {
        f.send(Frame::FollowStatus {
            running: false,
            // The session is gone; nothing on a screen to read any more.
            state: AgentState::Unknown,
            idle_ms: now_ms().saturating_sub(meta.last_output_ms.load(Ordering::Relaxed)),
        });
        f.send(Frame::Error {
            code: code::SESSION_EXITED,
            msg: format!("session '{name}' exited"),
        });
    }
    meta.attached_clients.store(0, Ordering::Relaxed);
    info!(session = %name, "session ended");
}

fn merge_terminal_appearance(
    current: TerminalAppearance,
    offered: TerminalAppearance,
) -> TerminalAppearance {
    TerminalAppearance {
        foreground: current.foreground.or(offered.foreground),
        background: current.background.or(offered.background),
    }
}

fn vt_rgb(color: TerminalColor) -> Rgb {
    Rgb {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

fn flush_pty_responses(
    vt: &mut GhosttyVt,
    writer: &mut Box<dyn Write + Send>,
) -> std::io::Result<()> {
    let responses = vt.take_pty_responses();
    if !responses.is_empty() {
        writer.write_all(&responses)?;
        writer.flush()?;
    }
    Ok(())
}

fn write_script_input<W, P>(
    writer: &mut W,
    bytes: &[u8],
    enter: bool,
    mut pause: P,
) -> std::io::Result<()>
where
    W: Write + ?Sized,
    P: FnMut(std::time::Duration),
{
    if !bytes.is_empty() {
        writer.write_all(bytes)?;
        writer.flush()?;
    }
    if enter {
        if !bytes.is_empty() {
            pause(SCRIPT_ENTER_DELAY);
        }
        writer.write_all(b"\r")?;
        writer.flush()?;
    }
    Ok(())
}

/// The pty size every attached client has to live with.
///
/// One pty, many viewers: it can only be as large as the smallest window
/// looking at it, or a client would be sent content it has no room to show and
/// would have to letterbox — which is what left stale cells stranded on screen
/// before. Taking the minimum per axis is also the only rule that does not
/// depend on who moved last, and it grows back the moment the small window
/// closes.
fn negotiated_size(sizes: &std::collections::HashMap<u64, (u16, u16)>) -> Option<(u16, u16)> {
    sizes
        .values()
        .copied()
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
}

/// Drop sizes of clients that are gone, then resize the pty to what the
/// remaining ones agree on. With no viewers left the last size stands — a
/// session nobody is watching keeps running at the size it had.
fn resize_to_clients(
    master: &(dyn portable_pty::MasterPty + Send),
    vt: &mut GhosttyVt,
    meta: &SessionMeta,
    clients: &[ClientSink],
    sizes: &mut std::collections::HashMap<u64, (u16, u16)>,
) {
    sizes.retain(|id, _| clients.iter().any(|c| c.id == *id));
    if let Some((cols, rows)) = negotiated_size(sizes) {
        apply_resize(master, vt, meta, cols, rows);
    }
}

fn apply_resize(
    master: &(dyn portable_pty::MasterPty + Send),
    vt: &mut GhosttyVt,
    meta: &SessionMeta,
    cols: u16,
    rows: u16,
) {
    if cols == 0 || rows == 0 {
        return;
    }
    if master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_err()
    {
        return;
    }
    vt.resize(cols, rows);
    meta.cols.store(cols, Ordering::Relaxed);
    meta.rows.store(rows, Ordering::Relaxed);
}

/// Shortest gap between two detections. A redrawing agent produces output
/// continuously, so detection is throttled rather than run per batch; a batch
/// that arrives inside the window sets a pending flag instead, and the loop
/// wakes to service it. Without that flag the *last* batch of a turn — the one
/// that draws the finished screen — could be the one skipped, leaving a
/// session reported as working after it stopped.
const DETECT_INTERVAL_MS: u64 = 250;

/// The visible screen as plain-text lines: the same screen-space path `peek`
/// renders, without the scrollback above it.
fn screen_lines(vt: &mut GhosttyVt, rows: u16) -> Vec<String> {
    let start = vt.scrollback_rows() as u32;
    vt.fetch_history(start, u32::from(rows))
        .into_iter()
        .map(|row| String::from_utf8_lossy(&row).into_owned())
        .collect()
}

/// Re-read the screen and publish what the program on it is doing.
///
/// Runs on the session thread, which exclusively owns the terminal model, so
/// the screen it reads is never a half-drawn frame observed from outside. The
/// agent is resolved from the pty's foreground process each time rather than
/// remembered: a session's occupant changes when a program is started or
/// exits, and a stale agent id would keep applying one agent's rules to
/// another's screen.
fn update_agent_state(detector: &Detector, vt: &mut GhosttyVt, meta: &SessionMeta) {
    let command =
        foreground_command(meta.pty_master_fd.load(Ordering::Relaxed)).unwrap_or_default();
    let title = vt.title();
    let lines = screen_lines(vt, meta.rows.load(Ordering::Relaxed));
    let screen = crate::detect::Screen {
        title: &title,
        lines: &lines,
    };
    // The rule, not just its verdict: a state nobody expected is only
    // debuggable if the daemon can say which rule produced it.
    let (state, matched) = detector.detect(&command, &screen);
    if let Ok(mut shared) = meta.state.lock()
        && *shared != state
    {
        debug!(
            session = %meta.name.lock().map(|n| n.clone()).unwrap_or_default(),
            %state,
            rule = matched.map(|rule| rule.id.as_str()).unwrap_or("none"),
            "agent state changed"
        );
        *shared = state;
    }
}

/// Render the session's screen as a plain-text `PeekReply` (`asd peek`). The
/// visible screen is the bottom `rows` of screen space; `scrollback` prepends
/// history above it — all of it, or the last N lines. Rows come from the same
/// screen-space plain-text path as the scrollback fetch; trailing blank lines
/// are trimmed (boo's peek behavior).
fn render_peek(vt: &mut GhosttyVt, scrollback: asd_proto::Scrollback) -> Frame {
    let snap = vt.render_snapshot();
    let (cursor_col, cursor_row) = snap.cursor.position.unwrap_or((0, 0));
    let (cols, rows) = (snap.cols, snap.rows);
    let history = vt.scrollback_rows() as u32;
    let total = vt.history_len() as u32;
    let (start, count) = match scrollback {
        asd_proto::Scrollback::None => (history, u32::from(rows)),
        asd_proto::Scrollback::All => (0, total),
        // Counted back from where the screen begins, so the screen itself is
        // always included; more lines than exist just means all of them.
        asd_proto::Scrollback::Lines(n) => {
            let start = history.saturating_sub(n);
            (start, total - start)
        }
    };
    let lines = vt.fetch_history(start, count);
    // Trim trailing blank lines (empty or all-spaces), then join with '\n'.
    let mut end = lines.len();
    while end > 0 && lines[end - 1].iter().all(|&b| b == b' ') {
        end -= 1;
    }
    let screen = lines[..end].join(&b'\n');
    Frame::PeekReply {
        cols,
        rows,
        cursor_col,
        cursor_row,
        title: vt.title(),
        screen,
    }
}

/// Where the session stands, as a `FollowStatus` plus the `running` flag it
/// carries.
///
/// `running` is `idle_ms < IDLE_SETTLE_MS`, read off the same
/// `last_output_ms` stamp that feeds `SessionInfo.running` and, through it,
/// `asd wait --idle`. One stamp and one rule, so the three ways of asking "is
/// it still working?" cannot come apart.
fn follow_status(meta: &SessionMeta) -> (Frame, bool) {
    let idle_ms = now_ms().saturating_sub(meta.last_output_ms.load(Ordering::Relaxed));
    let running = idle_ms < IDLE_SETTLE_MS;
    let state = meta.state.lock().map(|s| *s).unwrap_or_default();
    (
        Frame::FollowStatus {
            running,
            state,
            idle_ms,
        },
        running,
    )
}

/// Send the current status to every follower, dropping the ones that are gone.
/// Returns whether that status said the session had gone quiet — the caller
/// uses it to avoid saying so again until there is more output.
///
/// Followers are not attached clients, so this deliberately does not touch
/// `attached_clients`.
fn notify_followers(followers: &mut Vec<ClientSink>, meta: &SessionMeta) -> bool {
    let (status, running) = follow_status(meta);
    followers.retain(|f| f.send(status.clone()));
    !running
}

fn retain_live_clients(clients: &mut Vec<ClientSink>, frame: Frame) -> usize {
    let before = clients.len();
    clients.retain(|client| client.send(frame.clone()));
    before - clients.len()
}

fn remove_client_membership(
    client_id: u64,
    clients: &mut Vec<ClientSink>,
    tui_owner: &mut Option<TuiOwner>,
    client_sizes: &mut std::collections::HashMap<u64, (u16, u16)>,
) -> bool {
    let before = clients.len();
    clients.retain(|client| client.id != client_id);
    client_sizes.remove(&client_id);
    if tui_owner.is_some_and(|owner| owner.client_id == client_id) {
        *tui_owner = None;
    }
    clients.len() != before
}

fn broadcast(clients: &mut Vec<ClientSink>, meta: &SessionMeta, frame: Frame) -> usize {
    let dropped = retain_live_clients(clients, frame);
    meta.attached_clients
        .store(clients.len() as u32, Ordering::Relaxed);
    dropped
}

/// Kill the session's child process (ignored when the pid is already zeroed).
/// `force`: false = graceful (SIGHUP / best-effort CTRL_BREAK), true = force
/// (SIGKILL / TerminateProcess). The platform difference lives in
/// `platform::kill_child`.
pub fn kill_child(meta: &SessionMeta, force: bool) {
    let pid = meta.child_pid.load(Ordering::Relaxed);
    if pid == 0 {
        return;
    }
    crate::platform::kill_child(pid, force);
}

/// Ask a child to stop, then force it after the same grace period as `Kill`.
/// Used for an explicit kill and for a broken PTY response path: continuing a
/// session whose terminal queries can no longer be answered leaves the child
/// blocked indefinitely with no usable terminal channel.
fn request_child_shutdown(meta: &Arc<SessionMeta>) {
    kill_child(meta, false);
    let meta = Arc::clone(meta);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if meta.alive.load(Ordering::Relaxed) {
            kill_child(&meta, true);
        }
    });
}

// The only tests here exercise the macOS argv parser, which is compiled on
// Linux for exactly that purpose; on any other target there is nothing to test.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Build a synthetic `KERN_PROCARGS2` blob: argc, exec path, NUL padding,
    /// then the argv strings (env, which the parser ignores, follows).
    fn procargs2_blob(exec: &str, argv: &[&str], pad: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(argv.len() as i32).to_ne_bytes());
        b.extend_from_slice(exec.as_bytes());
        b.push(0);
        b.extend(std::iter::repeat_n(0u8, pad));
        for a in argv {
            b.extend_from_slice(a.as_bytes());
            b.push(0);
        }
        b.extend_from_slice(b"HOME=/x\0"); // trailing env, must be ignored
        b
    }

    #[test]
    fn parse_procargs2_basenames_argv0_and_keeps_args() {
        let blob = procargs2_blob("/usr/bin/node", &["/usr/bin/node", "vite", "serve"], 3);
        assert_eq!(parse_procargs2(&blob).as_deref(), Some("node vite serve"));
    }

    #[test]
    fn parse_procargs2_unwraps_sh_c_and_strips_login_dash() {
        // Our `--cmd` wrapper: sh -c "sleep 300" → "sleep 300".
        let blob = procargs2_blob("/bin/sh", &["/bin/sh", "-c", "sleep 300"], 1);
        assert_eq!(parse_procargs2(&blob).as_deref(), Some("sleep 300"));
        // A login shell's leading '-' is stripped.
        let blob = procargs2_blob("/bin/zsh", &["-zsh"], 0);
        assert_eq!(parse_procargs2(&blob).as_deref(), Some("zsh"));
    }

    #[test]
    fn parse_procargs2_rejects_malformed() {
        assert_eq!(parse_procargs2(&[]), None); // too short for argc
        assert_eq!(parse_procargs2(&0i32.to_ne_bytes()), None); // argc = 0
    }
}

#[cfg(test)]
mod appearance_tests {
    use super::*;
    use asd_proto::{TerminalAppearance, TerminalColor};

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "synthetic PTY failure",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn color(r: u8, g: u8, b: u8) -> TerminalColor {
        TerminalColor { r, g, b }
    }

    #[test]
    fn first_known_terminal_color_wins_per_channel() {
        let first = TerminalAppearance {
            foreground: None,
            background: Some(color(1, 2, 3)),
        };
        let second = TerminalAppearance {
            foreground: Some(color(4, 5, 6)),
            background: Some(color(7, 8, 9)),
        };

        let adopted = merge_terminal_appearance(TerminalAppearance::default(), first);
        let adopted = merge_terminal_appearance(adopted, second);

        assert_eq!(
            adopted,
            TerminalAppearance {
                foreground: Some(color(4, 5, 6)),
                background: Some(color(1, 2, 3)),
            }
        );
    }

    #[test]
    fn pty_response_write_errors_are_returned_to_the_session_loop() {
        let mut vt = GhosttyVt::new(10, 3, 0);
        vt.feed(b"\x1b[6n");
        let mut writer: Box<dyn Write + Send> = Box::new(FailingWriter);

        let error = flush_pty_responses(&mut vt, &mut writer).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;

    #[test]
    fn failed_broadcast_reports_the_removed_client() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let queued = Arc::new(AtomicUsize::new(0));
        let mut clients = vec![ClientSink::new(7, tx, queued)];

        assert_eq!(retain_live_clients(&mut clients, Frame::Ack), 1);
        assert!(clients.is_empty());
    }

    #[test]
    fn stale_attach_name_is_retagged_before_the_tui_snapshot() {
        assert_eq!(
            attach_view_rename(AttachClass::ExclusiveTui, "old", "new", 11),
            Some(Frame::ViewRenamed {
                old_name: "old".to_string(),
                new_name: "new".to_string(),
                view_id: 11,
            })
        );
        assert_eq!(
            attach_view_rename(AttachClass::Shared, "old", "new", 0),
            None
        );
        assert_eq!(
            attach_view_rename(AttachClass::ExclusiveTui, "same", "same", 11),
            None
        );
    }

    #[test]
    fn failed_direct_reply_removes_membership_owner_and_size() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let mut clients = vec![ClientSink::new(7, tx, queued)];
        let mut owner = Some(TuiOwner {
            client_id: 7,
            view_id: 11,
        });
        let mut sizes = std::collections::HashMap::from([(7, (40, 10))]);

        assert!(remove_client_membership(
            7,
            &mut clients,
            &mut owner,
            &mut sizes
        ));
        assert!(clients.is_empty());
        assert!(owner.is_none());
        assert!(sizes.is_empty());
    }
}

#[cfg(test)]
mod scripting_tests {
    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn scripted_enter_is_a_separate_pty_write_after_the_quiet_gap() {
        let mut writer = RecordingWriter::default();
        let mut pauses = Vec::new();

        write_script_input(&mut writer, b"do work", true, |duration| {
            pauses.push(duration)
        })
        .unwrap();

        assert_eq!(writer.writes, vec![b"do work".to_vec(), b"\r".to_vec()]);
        assert_eq!(writer.flushes, 2);
        assert_eq!(pauses, vec![SCRIPT_ENTER_DELAY]);
    }

    #[test]
    fn enter_without_a_payload_has_no_artificial_delay() {
        let mut writer = RecordingWriter::default();
        let mut pauses = Vec::new();

        write_script_input(&mut writer, b"", true, |duration| pauses.push(duration)).unwrap();

        assert_eq!(writer.writes, vec![b"\r".to_vec()]);
        assert_eq!(writer.flushes, 1);
        assert!(pauses.is_empty());
    }
}

#[cfg(test)]
mod session_env_tests {
    use super::*;

    /// The child is told which daemon owns it, so `asd` run inside a session
    /// reaches that daemon even when it listens somewhere non-default.
    #[test]
    fn session_env_carries_the_daemons_own_socket() {
        let mut builder = CommandBuilder::new("/bin/sh");

        set_session_env(
            &mut builder,
            "web",
            std::path::Path::new("/custom/asd.sock"),
        );

        assert_eq!(builder.get_env("ASD_SESSION").unwrap(), "web");
        assert_eq!(builder.get_env("ASD_SOCKET").unwrap(), "/custom/asd.sock");
        assert_eq!(builder.get_env("TERM").unwrap(), "xterm-256color");
    }
}
