//! Integration tests (spec §8): real UDS + real daemon process + the `asd` CLI.
//!
//! One test binary, split by subject: `common` holds the harness (an isolated
//! daemon, a raw protocol client, waiting helpers) and every other module owns
//! one slice of the surface — see each module's own header.

mod common;

mod attach;
mod cli;
mod daemon;
mod follow;
mod scripting;
mod status;
mod terminal;
