//! Process exit statuses.
//!
//! The daemon answers failures with a protocol code (`asd_proto::code`), but
//! until now every one of them reached the shell as a bare exit 1, with the
//! code surviving only as text inside the stderr message. A caller that wanted
//! to tell "no such session" from "the daemon is down" had to match on wording.
//!
//! So a daemon error carries its code to the process boundary, and [`status`]
//! turns it into an exit code. Only distinctions a caller can act on get their
//! own number — everything else stays 1, which is what a shell already assumes
//! about an unexplained failure:
//!
//! | status | meaning |
//! |--------|---------|
//! | 0 | success |
//! | 1 | anything else that failed |
//! | 3 | the named session does not exist (`asd`'s predecessor boo used 3 too) |
//! | 4 | `wait` timed out |
//!
//! 2 is deliberately unused: the shell convention for "wrong usage" is close
//! enough to clap's own exit that claiming it would be confusing.

use asd_proto::code;

/// `wait` gave up before its condition held.
pub(crate) const TIMEOUT: i32 = 4;
/// The session named on the command line does not exist.
pub(crate) const NO_SESSION: i32 = 3;

/// A daemon `Error` frame on its way out of the process, keeping the protocol
/// code next to the message built for the user.
#[derive(Debug)]
struct Coded {
    code: u32,
    message: String,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

/// The daemon refused: report it in the usual `{verb} failed ({code}): {msg}`
/// shape, and keep the code so [`status`] can act on it.
pub(crate) fn daemon(verb: &str, code: u32, msg: &str) -> anyhow::Error {
    Coded {
        code,
        message: format!("{verb} failed ({code}): {msg}"),
    }
    .into()
}

/// Exit status for a failed run.
pub fn status(err: &anyhow::Error) -> i32 {
    match err.downcast_ref::<Coded>() {
        Some(c) if c.code == code::NO_SUCH_SESSION => NO_SESSION,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_session_gets_its_own_status() {
        let e = daemon("peek", code::NO_SUCH_SESSION, "no such session 'x'");
        assert_eq!(status(&e), NO_SESSION);
        // The wording a caller sees is unchanged by carrying the code.
        assert_eq!(e.to_string(), "peek failed (2): no such session 'x'");
    }

    #[test]
    fn other_daemon_errors_stay_generic() {
        for c in [
            code::SESSION_EXISTS,
            code::INVALID_NAME,
            code::ALREADY_ATTACHED,
            code::INTERNAL,
        ] {
            assert_eq!(status(&daemon("create", c, "nope")), 1, "code {c}");
        }
    }

    #[test]
    fn a_plain_error_is_still_one() {
        assert_eq!(status(&anyhow::anyhow!("socket vanished")), 1);
    }

    /// The code has to survive being wrapped, since callers add context.
    #[test]
    fn context_does_not_hide_the_code() {
        let e = daemon("attach", code::NO_SUCH_SESSION, "no such session 'x'");
        let wrapped = e.context("while attaching");
        assert_eq!(status(&wrapped), NO_SESSION);
    }
}
