//! The git data layer. Deliberately free of ratatui: the lane layout is the
//! part worth testing hardest, and it must be testable without a terminal.

pub mod repo;

#[cfg(test)]
pub(crate) mod fixture;
