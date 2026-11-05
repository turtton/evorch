//! Integration-test view of `shared-test-support/`.
//!
//! The library's unit tests include the same module. It lives outside `tests/`
//! because a library unit that reads files under another target's directory is
//! checked by mtime and rebuilt in every new worktree.

#![allow(unused_imports)]

#[path = "../../shared-test-support/mod.rs"]
mod shared;

pub use shared::*;
