//! Rendering. Everything here is ratatui; nothing here decides lane layout,
//! which belongs to `crate::git::graph`. Pane layout — where the three panes
//! sit on screen — is `layout`.

pub mod colors;
pub mod graph_view;
pub mod layout;
