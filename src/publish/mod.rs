//! Publishing from a PC.
//!
//! This module intentionally has no station dependencies: the same code backs
//! `yam-publish` and the desktop build of `yam`. Network access happens only in
//! `client`, credentials only in `credentials`, and the publication rules are
//! pure local checks in `rules`.

pub mod cli;
pub mod client;
pub mod credentials;
pub mod rules;
pub mod sign;
