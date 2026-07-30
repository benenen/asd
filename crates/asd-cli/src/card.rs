//! `asd card` — what each session is working on, for an agent choosing where
//! to run something.
//!
//! A session list answers "what exists"; picking the right session needs "what
//! is this one *for*". A project already answers that in its own files, so a
//! card is the session's metadata plus the documents in its working directory:
//! `README.md`, `CLAUDE.md`, `AGENTS.md`, `CONTRIBUTING.md` — matched ignoring
//! case, since projects and filesystems disagree about it.
//!
//! Three levels, so a caller reads only as much as the decision needs:
//! [`list`] is one line per session (where it is, which documents it has),
//! [`inspect`] adds each document's heading and opening lines, and [`cat`]
//! prints one file in full.
//!
//! **Local daemon only.** The session's directory is read from the process
//! itself (`/proc/<pid>/cwd`), which only works where that process runs. For a
//! session on a remote daemon the pid means nothing here, so the card reports
//! the directory as unknown rather than guessing — and it is a guess worth
//! avoiding, since an unrelated local process could hold the same pid.

use std::path::{Path, PathBuf};

use anyhow::bail;
use asd_proto::{ClientKind, Frame, SessionInfo, code};

use crate::client;
use crate::control::json_string;
use crate::exit;

/// The files a project introduces itself with, in the order a card reports
/// them. Fixed on purpose: this is the set an agent knows to look for.
const DOC_FILES: &[&str] = &["README.md", "CLAUDE.md", "AGENTS.md", "CONTRIBUTING.md"];

/// How much of a document a card carries. Enough to tell projects apart;
/// `card cat` is there when the whole thing is wanted.
const EXCERPT_BYTES: usize = 1024;

/// How much of a file is read to build its heading and excerpt.
const SCAN_BYTES: u64 = 8 * 1024;

/// A project document as a card reports it.
pub(crate) struct Doc {
    pub(crate) file: String,
    pub(crate) bytes: u64,
    pub(crate) modified_ms: u64,
    /// The `# ` title, or empty when the document has none.
    pub(crate) heading: String,
    /// Opening prose, headings and blank runs removed.
    pub(crate) excerpt: String,
    /// The excerpt is not the whole document.
    pub(crate) truncated: bool,
}

/// `asd card list`: one line per session — where it is and what it holds.
pub async fn list(socket: &Path, json: bool) -> anyhow::Result<()> {
    let sessions = sessions(socket).await?;
    let cards: Vec<(SessionInfo, Option<PathBuf>, Vec<Doc>)> = sessions
        .into_iter()
        .map(|s| {
            let dir = session_dir(&s);
            let docs = dir.as_deref().map(scan_docs).unwrap_or_default();
            (s, dir, docs)
        })
        .collect();

    if json {
        let mut out = String::from("[");
        for (i, (s, dir, docs)) in cards.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r#"{"session":"#);
            json_string(&s.name, &mut out);
            out.push_str(r#","status":"#);
            json_string(status(s), &mut out);
            out.push_str(r#","cwd":"#);
            push_dir(dir.as_deref(), &mut out);
            out.push_str(r#","docs":["#);
            for (j, d) in docs.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                json_string(&d.file, &mut out);
            }
            out.push_str("]}");
        }
        out.push(']');
        println!("{out}");
        return Ok(());
    }

    if cards.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!(
        "{:<16} {:>8}  {}  CWD",
        "NAME",
        "STATUS",
        crate::pad_cell("DOCS", DOC_COL)
    );
    for (s, dir, docs) in &cards {
        let names = if docs.is_empty() {
            "-".to_string()
        } else {
            docs.iter()
                .map(|d| d.file.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        println!(
            "{:<16} {:>8}  {}  {}",
            s.name,
            status(s),
            crate::pad_cell(&names, DOC_COL),
            dir.as_deref()
                .map_or("unknown", |p| p.to_str().unwrap_or("?")),
        );
    }
    Ok(())
}

/// Width of the DOCS column: two filenames' worth, so the common case fits.
const DOC_COL: usize = 26;

/// `asd card inspect <name>`: one session's card, documents summarised.
pub async fn inspect(socket: &Path, name: String, json: bool) -> anyhow::Result<()> {
    let info = session(socket, &name).await?;
    let dir = session_dir(&info);
    let docs = dir.as_deref().map(scan_docs).unwrap_or_default();

    if json {
        let mut out = String::from(r#"{"session":"#);
        json_string(&info.name, &mut out);
        out.push_str(r#","status":"#);
        json_string(status(&info), &mut out);
        out.push_str(r#","command":"#);
        json_string(&info.command, &mut out);
        out.push_str(r#","title":"#);
        json_string(&info.title, &mut out);
        out.push_str(&format!(
            r#","pid":{},"cols":{},"rows":{},"cwd":"#,
            info.pid, info.cols, info.rows
        ));
        push_dir(dir.as_deref(), &mut out);
        out.push_str(r#","docs":["#);
        for (i, d) in docs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r#"{"file":"#);
            json_string(&d.file, &mut out);
            out.push_str(&format!(
                r#","bytes":{},"modified_ms":{},"heading":"#,
                d.bytes, d.modified_ms
            ));
            json_string(&d.heading, &mut out);
            out.push_str(r#","excerpt":"#);
            json_string(&d.excerpt, &mut out);
            out.push_str(&format!(r#","truncated":{}}}"#, d.truncated));
        }
        out.push_str("]}");
        println!("{out}");
        return Ok(());
    }

    let mut out = format!(
        "session    {name}\n\
         status     {status}\n\
         command    {command}\n",
        name = info.name,
        status = status(&info),
        command = info.command,
    );
    if !info.title.trim().is_empty() {
        out.push_str(&format!("title      {}\n", info.title));
    }
    out.push_str(&format!(
        "pid        {pid}\n\
         size       {cols}x{rows}\n\
         cwd        {cwd}\n",
        pid = info.pid,
        cols = info.cols,
        rows = info.rows,
        cwd = dir
            .as_deref()
            .map_or("unknown (not this machine?)".to_string(), |p| p
                .display()
                .to_string()),
    ));
    if docs.is_empty() {
        out.push_str("docs       none\n");
    }
    for d in &docs {
        out.push_str(&format!(
            "\n{file}  {size}  modified {age}\n",
            file = d.file,
            size = fmt_bytes(d.bytes),
            age = crate::format_age(d.modified_ms),
        ));
        if !d.heading.is_empty() {
            out.push_str(&format!("  # {}\n", d.heading));
        }
        for line in d.excerpt.lines() {
            out.push_str(&format!("  {line}\n"));
        }
        if d.truncated {
            out.push_str("  …\n");
        }
    }
    print!("{out}");
    Ok(())
}

/// `asd card cat <name> <path>`: one file from the session's directory.
pub async fn cat(socket: &Path, name: String, path: String, json: bool) -> anyhow::Result<()> {
    let info = session(socket, &name).await?;
    let Some(dir) = session_dir(&info) else {
        bail!("card: cannot resolve the working directory of session '{name}' on this machine");
    };
    let file = resolve_in(&dir, &path)?;
    let bytes = std::fs::read(&file)
        .map_err(|e| anyhow::anyhow!("card: reading {}: {e}", file.display()))?;

    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    if !json {
        stdout.write_all(&bytes)?;
        return Ok(());
    }
    let mut out = String::from(r#"{"session":"#);
    json_string(&info.name, &mut out);
    out.push_str(r#","path":"#);
    json_string(&file.display().to_string(), &mut out);
    out.push_str(&format!(r#","bytes":{},"content":"#, bytes.len()));
    json_string(&String::from_utf8_lossy(&bytes), &mut out);
    out.push_str("}\n");
    stdout.write_all(out.as_bytes())?;
    Ok(())
}

/// Resolve `path` inside a session's directory, ignoring case.
///
/// Escaping the directory is refused. That is a guardrail, not a privilege
/// boundary — whoever runs `asd` can read those files anyway — but `card` is
/// built for agents, and an agent following a path out of the project it was
/// pointed at is a mistake worth making impossible rather than likely.
pub(crate) fn resolve_in(dir: &Path, path: &str) -> anyhow::Result<PathBuf> {
    // `join` on an absolute path replaces the base, so this also covers
    // `card cat sess /etc/passwd`.
    let target = dir.join(path);
    let root = dir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("card: {}: {e}", dir.display()))?;
    let file = match target.canonicalize() {
        Ok(file) => file,
        // Not there as spelled: the same name in another case is the same
        // file to whoever asked, and here that is an agent repeating a name
        // out of a document rather than off the disk.
        Err(e) => ignoring_case(dir, path)
            .and_then(|p| p.canonicalize().ok())
            .ok_or_else(|| anyhow::anyhow!("card: {}: {e}", target.display()))?,
    };
    if !file.starts_with(&root) {
        bail!(
            "card: {} is outside the session's directory ({})",
            file.display(),
            root.display()
        );
    }
    Ok(file)
}

/// `path` under `dir` with every component matched ignoring case, or `None`
/// when no such file is there.
///
/// Only descends: `..`, a root, or a drive prefix abandons the search rather
/// than being followed case-blind, so widening the spelling never widens the
/// reach. The caller's containment check still has the last word.
fn ignoring_case(dir: &Path, path: &str) -> Option<PathBuf> {
    use std::path::Component;

    let mut at = dir.to_path_buf();
    for c in Path::new(path).components() {
        match c {
            Component::CurDir => {}
            Component::Normal(name) => at.push(match_name(&entries(&at), name.to_str()?)?),
            _ => return None,
        }
    }
    Some(at)
}

/// The documents present in a session's directory.
pub(crate) fn scan_docs(dir: &Path) -> Vec<Doc> {
    let present = entries(dir);
    let mut docs = Vec::new();
    for file in DOC_FILES {
        let Some(name) = match_name(&present, file) else {
            continue;
        };
        let path = dir.join(&name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let text = read_head(&path, SCAN_BYTES).unwrap_or_default();
        let (heading, excerpt, cut) = summarize(&text);
        docs.push(Doc {
            file: name,
            bytes: meta.len(),
            modified_ms: modified_ms(&meta),
            heading,
            excerpt,
            truncated: cut || meta.len() > SCAN_BYTES,
        });
    }
    docs
}

/// The names in a directory, unordered (an unreadable directory has none).
fn entries(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// The name in `present` that spells `want`, ignoring case.
///
/// Projects do not agree on capitalisation (`readme.md`, `Readme.md`), and on
/// macOS or Windows the filesystem does not either — so a card that only
/// matched the canonical spelling would report "no documents" for a project
/// that has them. The name is returned as written on disk: it is what `card
/// cat` is handed back, and there the path must match exactly.
///
/// An exact match wins over a differently-cased one, which is the only stable
/// answer when a case-sensitive filesystem holds both — directory order is not.
fn match_name(present: &[String], want: &str) -> Option<String> {
    if present.iter().any(|n| n == want) {
        return Some(want.to_string());
    }
    let mut hits: Vec<&String> = present
        .iter()
        .filter(|n| n.eq_ignore_ascii_case(want))
        .collect();
    hits.sort();
    hits.first().map(|n| (*n).to_string())
}

/// A document's `# ` title and opening prose, plus whether prose was cut.
///
/// Section headings and repeated blank lines are dropped: what identifies a
/// project is the sentence under the title, and a card that spent its budget
/// on a table of contents would say nothing.
pub(crate) fn summarize(text: &str) -> (String, String, bool) {
    let heading = text
        .lines()
        // A title is at the top or nowhere; further `#` lines are sections.
        .take(20)
        .find_map(|l| l.trim().strip_prefix("# ").map(|h| h.trim().to_string()))
        .unwrap_or_default();

    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        // Drops leading blanks (nothing pushed yet) and collapses runs.
        if line.trim().is_empty() && lines.last().is_none_or(|p| p.trim().is_empty()) {
            continue;
        }
        lines.push(line);
    }
    let body = lines.join("\n");
    let body = body.trim_end();

    let mut end = EXCERPT_BYTES.min(body.len());
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    (
        heading,
        body[..end].trim_end().to_string(),
        end < body.len(),
    )
}

/// Every session the daemon knows about.
async fn sessions(socket: &Path) -> anyhow::Result<Vec<SessionInfo>> {
    let mut c = client::connect(socket, ClientKind::Cli).await?;
    c.writer.write_frame(&Frame::ListSessions).await?;
    match c.reader.read_frame().await? {
        Some(Frame::SessionList { sessions }) => Ok(sessions),
        Some(Frame::Error { code, msg }) => Err(exit::daemon("card", code, &msg)),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// One session by name, with the same missing-session status the rest of the
/// CLI reports (exit 3).
async fn session(socket: &Path, name: &str) -> anyhow::Result<SessionInfo> {
    sessions(socket)
        .await?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| {
            exit::daemon(
                "card",
                code::NO_SUCH_SESSION,
                &format!("no such session '{name}'"),
            )
        })
}

/// Where a session is working, or `None` when its process is not on this
/// machine (or the platform cannot say).
fn session_dir(info: &SessionInfo) -> Option<PathBuf> {
    asd_daemon::read_cwd(info.pid)
}

fn status(info: &SessionInfo) -> &'static str {
    if info.running { "running" } else { "idle" }
}

fn push_dir(dir: Option<&Path>, out: &mut String) {
    match dir {
        Some(p) => json_string(&p.display().to_string(), out),
        None => out.push_str("null"),
    }
}

/// The first `max` bytes of a file as text (lossy: a document is text, but a
/// cut at `max` can land inside a character).
fn read_head(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fmt_bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "asd-card-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn summarize_takes_the_title_and_the_prose_under_it() {
        let (heading, excerpt, truncated) = summarize(
            "# asd\n\n\n\nA terminal session multiplexer.\n\n## Features\n\n- persistent\n",
        );
        assert_eq!(heading, "asd");
        // Section headings are dropped, blank runs collapsed, prose kept.
        assert_eq!(excerpt, "A terminal session multiplexer.\n\n- persistent");
        assert!(!truncated);
    }

    #[test]
    fn summarize_handles_a_document_with_no_title() {
        let (heading, excerpt, _) = summarize("just prose, no heading\nsecond line\n");
        assert_eq!(heading, "");
        assert_eq!(excerpt, "just prose, no heading\nsecond line");
        // Nothing at all is not an error either.
        assert_eq!(summarize(""), (String::new(), String::new(), false));
    }

    #[test]
    fn summarize_cuts_a_long_document_on_a_character_boundary() {
        // Multi-byte text so a naive byte cut would split a character.
        let text = "# 标题\n\n".to_string() + &"中文内容。".repeat(500);
        let (heading, excerpt, truncated) = summarize(&text);
        assert_eq!(heading, "标题");
        assert!(truncated, "a 7 KB document should be cut");
        assert!(
            excerpt.len() <= EXCERPT_BYTES,
            "over budget: {}",
            excerpt.len()
        );
        // The cut is valid UTF-8 by construction — the test would not compile
        // otherwise — but check it did not lose the start.
        assert!(excerpt.starts_with("中文内容。"));
    }

    #[test]
    fn scan_docs_reports_the_documents_that_exist() {
        let dir = tempdir("scan");
        std::fs::write(dir.join("README.md"), "# proj\n\nWhat it does.\n").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# agents\n\nHow to drive it.\n").unwrap();
        // Not in the fixed set: ignored.
        std::fs::write(dir.join("NOTES.md"), "# notes\n").unwrap();
        // A directory with a document's name is not a document.
        std::fs::create_dir(dir.join("CLAUDE.md")).unwrap();

        let docs = scan_docs(&dir);
        let names: Vec<&str> = docs.iter().map(|d| d.file.as_str()).collect();
        assert_eq!(names, ["README.md", "AGENTS.md"], "docs: {names:?}");
        assert_eq!(docs[0].heading, "proj");
        assert_eq!(docs[0].excerpt, "What it does.");
        assert!(docs[0].bytes > 0);

        // A directory with none of them is empty, not an error.
        assert!(scan_docs(&tempdir("empty")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_docs_finds_documents_whatever_their_case() {
        let dir = tempdir("case");
        std::fs::write(dir.join("readme.md"), "# proj\n\nlower case.\n").unwrap();
        std::fs::write(dir.join("Claude.md"), "# c\n\nmixed case.\n").unwrap();

        let docs = scan_docs(&dir);
        // Reported in the fixed order, under the name the file actually has —
        // that name is what `card cat` is given.
        let names: Vec<&str> = docs.iter().map(|d| d.file.as_str()).collect();
        assert_eq!(names, ["readme.md", "Claude.md"], "docs: {names:?}");
        assert_eq!(docs[0].excerpt, "lower case.");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The rule that decides which spelling wins, tested where it lives: on a
    /// list of names. Pinning it through the filesystem instead would pin it
    /// only where two spellings can coexist — see the test below.
    #[test]
    fn match_name_prefers_the_exact_spelling() {
        let clash: Vec<String> = ["readme.md", "README.md", "Readme.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            match_name(&clash, "README.md").as_deref(),
            Some("README.md")
        );

        // No exact spelling: the lowest-sorting match, so the answer does not
        // depend on the order the directory happened to be read in.
        let inexact: Vec<String> = ["readme.md", "Readme.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            match_name(&inexact, "README.md").as_deref(),
            Some("Readme.md")
        );
        assert_eq!(match_name(&inexact, "AGENTS.md"), None);
    }

    #[test]
    fn scan_docs_prefers_the_exactly_named_document() {
        let dir = tempdir("case-clash");
        std::fs::write(dir.join("readme.md"), "# lower\n\nlower.\n").unwrap();
        std::fs::write(dir.join("README.md"), "# upper\n\nupper.\n").unwrap();

        let docs = scan_docs(&dir);
        let names: Vec<&str> = docs.iter().map(|d| d.file.as_str()).collect();
        // Whether there is a clash at all is the filesystem's call, so ask it
        // rather than the platform: both spellings survive on a case-sensitive
        // one (Linux), while Windows and a default macOS volume fold the two
        // writes into a single file. Either way the reported name must be the
        // one on disk, because that is the path `card cat` is handed.
        let on_disk = std::fs::read_dir(&dir).unwrap().count();
        if on_disk == 2 {
            assert_eq!(names, ["README.md"], "docs: {names:?}");
        } else {
            assert_eq!(names.len(), 1, "one file, one doc: {names:?}");
            assert!(dir.join(names[0]).is_file(), "reported {names:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_in_refuses_to_leave_the_session_directory() {
        let dir = tempdir("jail");
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();

        assert_eq!(
            resolve_in(&dir, "src/main.rs").unwrap(),
            dir.canonicalize().unwrap().join("src/main.rs")
        );
        // Traversal, absolute paths, and a missing file all fail rather than
        // reaching outside.
        for bad in ["../../../etc/passwd", "/etc/passwd", "nope.md"] {
            assert!(resolve_in(&dir, bad).is_err(), "allowed {bad}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_in_finds_a_path_whatever_its_case() {
        let dir = tempdir("case-cat");
        std::fs::create_dir(dir.join("Docs")).unwrap();
        std::fs::write(dir.join("Docs/Readme.md"), "# doc\n").unwrap();

        let want = dir.canonicalize().unwrap().join("Docs/Readme.md");
        // Every component is matched, not just the filename.
        assert_eq!(resolve_in(&dir, "Docs/Readme.md").unwrap(), want);
        assert_eq!(resolve_in(&dir, "docs/readme.md").unwrap(), want);
        assert_eq!(resolve_in(&dir, "DOCS/README.MD").unwrap(), want);
        assert_eq!(resolve_in(&dir, "./docs/readme.md").unwrap(), want);
        // A file that is not there is still missing, in any case.
        assert!(resolve_in(&dir, "docs/nope.md").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn case_insensitive_paths_still_cannot_leave_the_directory() {
        let dir = tempdir("case-jail");
        std::fs::write(dir.join("keep.md"), "x").unwrap();

        for bad in ["/ETC/passwd", "../../../ETC/passwd", "../CASE-JAIL/keep.md"] {
            assert!(resolve_in(&dir, bad).is_err(), "allowed {bad}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
