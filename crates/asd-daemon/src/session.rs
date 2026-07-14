//! session = PTY + child process + headless Terminal + scrollback (spec §5).
//!
//! Threading model: one std thread per session owns its Terminal exclusively
//! (`GhosttyVt` is `!Send`, so it cannot leave the thread — enforced at
//! compile time); pty reads, Input frames, and Resize all enter that thread
//! via a channel. The network side (tokio) holds only a [`SessionHandle`].

use std::io::Write;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use asd_proto::{Frame, code};
use asd_vt::{GhosttyVt, VtBackend};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tracing::{debug, info, warn};

use crate::registry::Registry;

/// Per-client Output send-queue cap (spec §5, M0-era flow control):
/// a full queue means the client is dead → disconnect it; the session is
/// unaffected.
pub const OUTPUT_QUEUE_CAP: usize = 4 * 1024 * 1024;

/// Scrollback line count (fixed in M0; moves to the config file from M1 on).
const SCROLLBACK_LINES: usize = 10_000;

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

/// Messages sent to the session thread.
pub enum SessionMsg {
    /// Raw output fed in by the pty read thread.
    PtyOutput(Vec<u8>),
    /// The pty read hit EOF/error — the end of the session's lifetime.
    PtyEof,
    /// Client input (already-encoded bytes), written to the pty.
    Input(Vec<u8>),
    /// Resize policy v1: "last Attach/Resize wins" (spec §5).
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Attach: reply with Snapshot first, then join the broadcast list; the
    /// ordering is guaranteed by the single channel.
    Attach {
        sink: ClientSink,
        cols: u16,
        rows: u16,
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
}

impl SessionHandle {
    pub fn info(&self) -> asd_proto::SessionInfo {
        // Report the live foreground command (what's actually running in the
        // terminal now), falling back to the spawn command when it can't be
        // resolved (session gone, or no /proc — e.g. non-Linux).
        let fd = self.meta.pty_master_fd.load(Ordering::Relaxed);
        let command = foreground_command(fd).unwrap_or_else(|| self.command.clone());
        asd_proto::SessionInfo {
            name: self.name.clone(),
            command,
            created_ms: self.created_ms,
            attached_clients: self.meta.attached_clients.load(Ordering::Relaxed),
            cols: self.meta.cols.load(Ordering::Relaxed),
            rows: self.meta.rows.load(Ordering::Relaxed),
        }
    }
}

/// The terminal's foreground command: the process group in the foreground of
/// the pty (`tcgetpgrp` on the master), resolved to a command line via `/proc`.
/// `None` when there is no foreground group or `/proc` is unavailable.
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

/// Format process `pid`'s command from `/proc/<pid>/cmdline` — argv[0]
/// basenamed (and de-`-`ed for login shells), remaining args kept — falling
/// back to `/proc/<pid>/comm`.
fn proc_command(pid: libc::pid_t) -> Option<String> {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let mut argv = raw.split(|&b| b == 0).filter(|s| !s.is_empty());
        if let Some(arg0) = argv.next() {
            let arg0 = String::from_utf8_lossy(arg0);
            let base = arg0.rsplit('/').next().unwrap_or(&arg0);
            let base = base.strip_prefix('-').unwrap_or(base); // login shell "-bash"
            let mut out = base.to_string();
            for arg in argv {
                out.push(' ');
                out.push_str(&String::from_utf8_lossy(arg));
            }
            // A `--cmd` session's foreground is our non-interactive `sh -c <c>`
            // wrapper (no job control, so sh stays the group leader). Show the
            // command it runs, not the wrapper. Interactive foreground jobs get
            // their own process group and never hit this.
            for prefix in ["sh -c ", "bash -c ", "dash -c ", "zsh -c "] {
                if let Some(rest) = out.strip_prefix(prefix) {
                    return Some(rest.to_string());
                }
            }
            return Some(out);
        }
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(c) if !c.trim().is_empty() => Some(c.trim().to_string()),
        _ => None,
    }
}

/// Create the pty, start the child process, and launch the session thread
/// and pty read thread.
pub fn spawn_session(
    name: String,
    cmd: Option<String>,
    cols: u16,
    rows: u16,
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
    builder.env("TERM", "xterm-256color");
    if let Some(home) = std::env::var_os("HOME") {
        builder.cwd(home);
    }

    let child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);
    let child_pid = child.process_id().unwrap_or(0);

    let master = pair.master;
    // Raw fd for foreground-process lookups; the master owns it and stays open
    // for the session's lifetime (this is a borrow, not a dup).
    let master_fd = master.as_raw_fd().unwrap_or(-1);
    let pty_writer = master.take_writer()?;
    let pty_reader = master.try_clone_reader()?;

    let (tx, rx) = mpsc::channel::<SessionMsg>();
    let meta = Arc::new(SessionMeta {
        cols: AtomicU16::new(cols),
        rows: AtomicU16::new(rows),
        attached_clients: AtomicU32::new(0),
        child_pid: AtomicU32::new(child_pid),
        alive: AtomicBool::new(true),
        pty_master_fd: AtomicI32::new(master_fd),
    });

    let created_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

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
                            let _ = tx.send(SessionMsg::PtyEof);
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

    // Session thread: exclusive owner of the Terminal and the pty master
    {
        let name = name.clone();
        let meta = Arc::clone(&meta);
        std::thread::Builder::new()
            .name(format!("session-{name}"))
            .spawn(move || {
                session_thread(
                    name, rx, master, pty_writer, child, cols, rows, meta, registry,
                );
            })?;
    }

    Ok(SessionHandle {
        name,
        command,
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
    meta: Arc<SessionMeta>,
    registry: Arc<Mutex<Registry>>,
) {
    let mut vt = GhosttyVt::new(cols, rows, SCROLLBACK_LINES);
    let mut clients: Vec<ClientSink> = Vec::new();
    info!(session = %name, pid = meta.child_pid.load(Ordering::Relaxed), "session started");

    while let Ok(msg) = rx.recv() {
        match msg {
            SessionMsg::PtyOutput(bytes) => {
                vt.feed(&bytes);
                // The terminal's replies to DA/DSR-style queries must be
                // written back to the pty, otherwise capability probes in
                // vim/htop hang
                let resp = vt.take_pty_responses();
                if !resp.is_empty() {
                    let _ = pty_writer.write_all(&resp);
                    let _ = pty_writer.flush();
                }
                broadcast(&mut clients, &meta, Frame::Output { bytes });
            }
            SessionMsg::Input(bytes) => {
                if pty_writer
                    .write_all(&bytes)
                    .and_then(|()| pty_writer.flush())
                    .is_err()
                {
                    debug!(session = %name, "pty write failed (child likely exited)");
                }
            }
            SessionMsg::Resize { cols, rows } => {
                apply_resize(&*master, &mut vt, &meta, cols, rows);
            }
            SessionMsg::Attach { sink, cols, rows } => {
                // Last attacher wins
                apply_resize(&*master, &mut vt, &meta, cols, rows);
                let snapshot = vt.snapshot_vt();
                // The Snapshot is enqueued before any subsequent Output (the
                // single channel preserves order)
                if sink.send(Frame::Snapshot { vt: snapshot }) {
                    clients.push(sink);
                    meta.attached_clients
                        .store(clients.len() as u32, Ordering::Relaxed);
                    info!(session = %name, clients = clients.len(), "client attached");
                }
            }
            SessionMsg::Detach { client_id } => {
                clients.retain(|c| c.id != client_id);
                meta.attached_clients
                    .store(clients.len() as u32, Ordering::Relaxed);
                debug!(session = %name, client = client_id, "client detached");
            }
            SessionMsg::FetchHistory { sink, start, count } => {
                let count = count.min(MAX_HISTORY_ROWS_PER_FETCH);
                let total_rows = vt.history_len() as u32;
                let rows = vt.fetch_history(start, count);
                // Reply on the requesting client's own sink. History is not a
                // data-plane frame, so it does not consume the flow-control
                // quota; the window is bounded by MAX_HISTORY_ROWS_PER_FETCH.
                sink.send(Frame::History {
                    total_rows,
                    start,
                    rows,
                });
            }
            SessionMsg::Refresh { sink } => {
                let snapshot = vt.snapshot_vt();
                sink.send(Frame::Snapshot { vt: snapshot });
            }
            SessionMsg::Kill => {
                info!(session = %name, "kill requested");
                signal_child(&meta, Signal::SIGHUP);
                // Follow up with SIGKILL after a 2s grace period (skipped if
                // the child already exited and the liveness check fails)
                let meta2 = Arc::clone(&meta);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if meta2.alive.load(Ordering::Relaxed) {
                        signal_child(&meta2, Signal::SIGKILL);
                    }
                });
            }
            SessionMsg::PtyEof => {
                info!(session = %name, "pty eof, session ending");
                break;
            }
        }
    }

    // Endpoint: reap the child, deregister, broadcast the exit, and
    // disconnect all clients
    let _ = child.wait();
    meta.alive.store(false, Ordering::Relaxed);
    meta.child_pid.store(0, Ordering::Relaxed);
    registry.lock().unwrap().remove(&name);
    for c in clients.drain(..) {
        c.send(Frame::Error {
            code: code::SESSION_EXITED,
            msg: format!("session '{name}' exited"),
        });
        // The sink is dropped by the drain; the connection side sees the
        // channel close after writing out the tail of its queue
    }
    meta.attached_clients.store(0, Ordering::Relaxed);
    info!(session = %name, "session ended");
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

fn broadcast(clients: &mut Vec<ClientSink>, meta: &SessionMeta, frame: Frame) {
    clients.retain(|c| c.send(frame.clone()));
    meta.attached_clients
        .store(clients.len() as u32, Ordering::Relaxed);
}

/// Signal the session's child process (ignored when the pid is already
/// zeroed).
pub fn signal_child(meta: &SessionMeta, sig: Signal) {
    let pid = meta.child_pid.load(Ordering::Relaxed);
    if pid != 0 {
        let _ = kill(Pid::from_raw(pid as i32), sig);
    }
}
