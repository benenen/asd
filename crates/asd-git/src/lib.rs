//! Commit-graph model and ratatui widget for the asd TUI's git overlay.
//!
//! This crate knows about a filesystem path, not about asd. It reaches
//! crossterm only through `ratatui::crossterm`, so a binary linking both this
//! and `asd-tui` cannot end up with two copies of crossterm's process-global
//! terminal state.

pub mod git;

pub use git::repo::{OpenError, Repo};
