//! Commit-graph model and ratatui widget for the asd TUI's git overlay.
//!
//! This crate knows about a filesystem path, not about asd. It reaches
//! crossterm only through `ratatui::crossterm`, so a binary linking both this
//! and `asd-tui` cannot end up with two copies of crossterm's process-global
//! terminal state.
//!
//! # What crosses the crate boundary
//!
//! The host names three things: [`GitGraph`], [`Outcome`] and [`OpenError`].
//! The renderers, the pane layout and the worker's request/reply plumbing are
//! `pub(crate)`: each is called from exactly one place, and every name kept
//! public is a name a later phase has to break rather than change. What is
//! re-exported below is the model a host could reasonably read — the states
//! [`GitGraph`]'s own accessors hand back, and the commit and diff facts
//! inside them.

pub mod git;
pub mod search;
pub mod state;
pub mod ui;
pub mod worker;

pub use git::commit::{CommitInfo, ReadError};
pub use git::diff::{CommitDiff, DiffLine, FileChange, FileDiff, FileStat, MAX_DIFF_LINES};
pub use git::graph::{CellType, GraphBuilder, GraphNode};
pub use git::refs::{RefInfo, RefKind};
pub use git::repo::{OpenError, Repo};
pub use search::{Search, rank};
pub use state::{DetailState, FileDiffState, GitGraph, Mode, Outcome, PAGE_FIRST};
pub use ui::colors::{LANE_COLORS, lane_color};
pub use worker::HighlightedDiff;
