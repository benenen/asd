//! Actual foreground executable/argv proof. Shell source is never interpreted.

use crate::agent_resume::AgentEvidence;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn foreground_agent(master_fd: i32) -> AgentEvidence {
    if master_fd < 0 {
        return AgentEvidence::Observed(None);
    }
    // SAFETY: the session owns this master fd; a stale fd fails closed.
    let pid = unsafe { libc::tcgetpgrp(master_fd) };
    let kind = (pid > 0)
        .then(|| read_process(pid))
        .flatten()
        .and_then(|(executable, argv)| crate::agent_resume::process_kind(&executable, &argv));
    AgentEvidence::Observed(kind)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn foreground_agent(_master_fd: i32) -> AgentEvidence {
    AgentEvidence::Unavailable
}

#[cfg(target_os = "linux")]
fn read_process(pid: libc::pid_t) -> Option<(String, Vec<String>)> {
    let path = format!("/proc/{pid}/exe");
    let executable = std::fs::read_link(&path).ok()?;
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.last() != Some(&0) || executable != std::fs::read_link(&path).ok()? {
        return None;
    }
    let argv = bytes[..bytes.len() - 1]
        .split(|b| *b == 0)
        .map(|word| std::str::from_utf8(word).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some((executable.to_str()?.to_owned(), argv))
}

#[cfg(target_os = "macos")]
fn read_process(pid: libc::pid_t) -> Option<(String, Vec<String>)> {
    let executable = read_macos_executable(pid)?;
    let mut argmax: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: mib and both output pointers have the indicated lengths.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || argmax <= 0 {
        return None;
    }
    let mut bytes = vec![0; argmax as usize];
    let mut len = bytes.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: bytes has len writable bytes; sysctl reports the initialized size.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    bytes.truncate(len);
    let (_, argv) = parse_macos_process(&bytes)?;
    if executable != read_macos_executable(pid)? {
        return None;
    }
    Some((executable, argv))
}

#[cfg(target_os = "macos")]
fn read_macos_executable(pid: libc::pid_t) -> Option<String> {
    let mut path = [0u8; 4096];
    // SAFETY: proc_pidpath receives the full size of this writable buffer.
    let count = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if count <= 0 {
        return None;
    }
    std::str::from_utf8(path.get(..count as usize)?)
        .ok()
        .map(str::to_owned)
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_process(bytes: &[u8]) -> Option<(String, Vec<String>)> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if argc <= 0 || argc as usize > bytes.len() {
        return None;
    }
    let mut remaining = &bytes[4..];
    let executable_end = remaining.iter().position(|b| *b == 0)?;
    let executable = std::str::from_utf8(&remaining[..executable_end])
        .ok()?
        .to_owned();
    if executable.is_empty() {
        return None;
    }
    remaining = &remaining[executable_end + 1..];
    while remaining.first() == Some(&0) {
        remaining = &remaining[1..];
    }
    let mut argv = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let end = remaining.iter().position(|b| *b == 0)?;
        argv.push(std::str::from_utf8(&remaining[..end]).ok()?.to_owned());
        remaining = &remaining[end + 1..];
    }
    Some((executable, argv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_process_arguments_remain_unflattened_and_shell_source_is_not_unwrapped() {
        let mut bytes = 3i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/sh\0\0sh\0-c\0codex --version; sleep 600\0ENV=value\0");
        let (executable, argv) = parse_macos_process(&bytes).unwrap();
        assert_eq!(executable, "/bin/sh");
        assert_eq!(argv, ["sh", "-c", "codex --version; sleep 600"]);
        assert!(parse_macos_process(&bytes[..10]).is_none());
        let mut malformed = bytes.clone();
        malformed[..4].copy_from_slice(&1000i32.to_ne_bytes());
        assert!(parse_macos_process(&malformed).is_none());
    }
}
