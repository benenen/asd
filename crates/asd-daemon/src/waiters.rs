//! Bounded, session-owned waits over each observed visible screen.

use std::time::Instant;

use asd_proto::{MAX_SCREEN_PATTERN, MAX_SCREEN_WAITERS, ScreenMatcher, SessionIdentity};
use tokio::sync::oneshot;

/// Bound expanded regex programs as well as source bytes. Counted repetitions
/// can expand a tiny pattern into a large compiled automaton.
const MAX_COMPILED_REGEX: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    #[error("invalid screen matcher: {0}")]
    InvalidMatcher(String),
    #[error("session already has {MAX_SCREEN_WAITERS} active screen waiters")]
    Limit,
    #[error("screen wait timed out")]
    TimedOut,
    #[error("session exited while waiting for screen")]
    SessionExited,
}

/// Compile on the connection side, before sending anything to the VT owner.
/// The CLI uses the same validation before opening a connection.
pub struct CompiledMatcher(Matcher);

enum Matcher {
    Literal(String),
    Regex(regex::Regex),
}

impl CompiledMatcher {
    pub fn compile(matcher: ScreenMatcher) -> Result<Self, WaitError> {
        let pattern = match &matcher {
            ScreenMatcher::Literal(pattern) | ScreenMatcher::Regex(pattern) => pattern,
        };
        if pattern.len() > MAX_SCREEN_PATTERN {
            return Err(WaitError::InvalidMatcher(format!(
                "pattern exceeds {MAX_SCREEN_PATTERN} bytes"
            )));
        }
        match matcher {
            ScreenMatcher::Literal(pattern) => Ok(Self(Matcher::Literal(pattern))),
            ScreenMatcher::Regex(pattern) => regex::RegexBuilder::new(&pattern)
                .size_limit(MAX_COMPILED_REGEX)
                .dfa_size_limit(MAX_COMPILED_REGEX)
                .build()
                .map(|regex| Self(Matcher::Regex(regex)))
                .map_err(|error| WaitError::InvalidMatcher(error.to_string())),
        }
    }

    fn is_match(&self, screen: &str) -> bool {
        match &self.0 {
            Matcher::Literal(pattern) => screen.contains(pattern),
            Matcher::Regex(regex) => regex.is_match(screen),
        }
    }
}

pub(crate) type WaitReply = oneshot::Sender<Result<SessionIdentity, WaitError>>;

struct Waiter {
    identity: SessionIdentity,
    matcher: CompiledMatcher,
    deadline: Instant,
    reply: WaitReply,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegisterResult {
    Matched,
    Registered,
}

#[derive(Default)]
pub(crate) struct ScreenWaiters(Vec<Waiter>);

impl ScreenWaiters {
    pub(crate) fn register(
        &mut self,
        identity: SessionIdentity,
        matcher: CompiledMatcher,
        deadline: Instant,
        screen: &str,
        reply: WaitReply,
    ) -> Result<RegisterResult, WaitError> {
        self.expire(Instant::now());
        if matcher.is_match(screen) {
            let _ = reply.send(Ok(identity));
            return Ok(RegisterResult::Matched);
        }
        if self.0.len() >= MAX_SCREEN_WAITERS {
            let _ = reply.send(Err(WaitError::Limit));
            return Err(WaitError::Limit);
        }
        self.0.push(Waiter {
            identity,
            matcher,
            deadline,
            reply,
        });
        Ok(RegisterResult::Registered)
    }

    pub(crate) fn observe(&mut self, screen: &str, now: Instant) {
        self.expire(now);
        let mut pending = Vec::with_capacity(self.0.len());
        for waiter in self.0.drain(..) {
            if waiter.matcher.is_match(screen) {
                let _ = waiter.reply.send(Ok(waiter.identity));
            } else {
                pending.push(waiter);
            }
        }
        self.0 = pending;
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        let mut pending = Vec::with_capacity(self.0.len());
        for waiter in self.0.drain(..) {
            if waiter.reply.is_closed() {
                continue;
            }
            if waiter.deadline <= now {
                let _ = waiter.reply.send(Err(WaitError::TimedOut));
            } else {
                pending.push(waiter);
            }
        }
        self.0 = pending;
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.0.iter().map(|waiter| waiter.deadline).min()
    }

    pub(crate) fn session_exited(&mut self) {
        for waiter in self.0.drain(..) {
            let _ = waiter.reply.send(Err(WaitError::SessionExited));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asd_proto::{MAX_SCREEN_WAITERS, ScreenMatcher, SessionIdentity};
    use std::time::{Duration, Instant};
    use tokio::sync::oneshot;

    fn identity() -> SessionIdentity {
        "00000000000000000000000000000001".parse().unwrap()
    }

    fn literal(text: &str) -> CompiledMatcher {
        CompiledMatcher::compile(ScreenMatcher::Literal(text.into())).unwrap()
    }

    #[test]
    fn current_screen_matches_without_registration() {
        let mut waits = ScreenWaiters::default();
        let (tx, mut rx) = oneshot::channel();
        assert_eq!(
            waits.register(identity(), literal("ready"), Instant::now(), "ready", tx),
            Ok(RegisterResult::Matched)
        );
        assert_eq!(rx.try_recv(), Ok(Ok(identity())));
        assert_eq!(waits.next_deadline(), None);
    }

    #[test]
    fn future_literal_and_regex_match_a_single_observed_revision() {
        let mut waits = ScreenWaiters::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        let matchers = [
            literal("MARK-42"),
            CompiledMatcher::compile(ScreenMatcher::Regex("MARK-[0-9]+".into())).unwrap(),
        ];
        let mut replies = Vec::new();
        for matcher in matchers {
            let (tx, rx) = oneshot::channel();
            assert_eq!(
                waits.register(identity(), matcher, deadline, "empty", tx),
                Ok(RegisterResult::Registered)
            );
            replies.push(rx);
        }
        assert_eq!(waits.next_deadline(), Some(deadline));
        waits.observe("MARK-42", Instant::now());
        waits.observe("erased", Instant::now());
        for mut reply in replies {
            assert_eq!(reply.try_recv(), Ok(Ok(identity())));
        }
        assert_eq!(waits.next_deadline(), None);
    }

    #[test]
    fn rejects_invalid_oversized_and_expensively_compiled_patterns() {
        for matcher in [
            ScreenMatcher::Regex("[".into()),
            ScreenMatcher::Regex("a".repeat(4097)),
            ScreenMatcher::Literal("é".repeat(2049)),
            ScreenMatcher::Regex("a{1000000}".into()),
        ] {
            assert!(matches!(
                CompiledMatcher::compile(matcher),
                Err(WaitError::InvalidMatcher(_))
            ));
        }
        assert!(CompiledMatcher::compile(ScreenMatcher::Literal("a".repeat(4096))).is_ok());
    }

    #[test]
    fn capacity_rejects_65th_then_reuses_disconnected_slot() {
        let mut waits = ScreenWaiters::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut replies = Vec::new();
        for _ in 0..MAX_SCREEN_WAITERS {
            let (tx, rx) = oneshot::channel();
            assert_eq!(
                waits.register(identity(), literal("later"), deadline, "", tx),
                Ok(RegisterResult::Registered)
            );
            replies.push(rx);
        }
        let (tx, mut rx) = oneshot::channel();
        assert_eq!(
            waits.register(identity(), literal("later"), deadline, "", tx),
            Err(WaitError::Limit)
        );
        assert_eq!(rx.try_recv(), Ok(Err(WaitError::Limit)));
        replies.pop();
        let (tx, _rx) = oneshot::channel();
        assert_eq!(
            waits.register(identity(), literal("later"), deadline, "", tx),
            Ok(RegisterResult::Registered)
        );
    }

    #[test]
    fn deadlines_expire_even_when_output_keeps_arriving() {
        let mut waits = ScreenWaiters::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        let (tx, mut rx) = oneshot::channel();
        waits
            .register(identity(), literal("late"), deadline, "", tx)
            .unwrap();
        waits.observe("late", deadline);
        assert_eq!(rx.try_recv(), Ok(Err(WaitError::TimedOut)));
        let (tx, mut rx) = oneshot::channel();
        waits
            .register(identity(), literal("later"), deadline, "", tx)
            .unwrap();
        waits.expire(deadline);
        assert_eq!(rx.try_recv(), Ok(Err(WaitError::TimedOut)));
    }

    #[test]
    fn prune_drops_closed_replies_and_exit_resolves_live_waiters() {
        let mut waits = ScreenWaiters::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        let (tx, rx) = oneshot::channel();
        waits
            .register(identity(), literal("later"), deadline, "", tx)
            .unwrap();
        drop(rx);
        waits.expire(Instant::now());
        assert_eq!(waits.next_deadline(), None);
        let (tx, mut rx) = oneshot::channel();
        waits
            .register(identity(), literal("later"), deadline, "", tx)
            .unwrap();
        waits.session_exited();
        assert_eq!(rx.try_recv(), Ok(Err(WaitError::SessionExited)));
        assert_eq!(waits.next_deadline(), None);
    }
}
