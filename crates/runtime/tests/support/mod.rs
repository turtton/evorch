//! Integration-test view of `shared-test-support/`.
//!
//! The library's unit tests include the same module. It lives outside `tests/`
//! because a library unit that reads files under another target's directory is
//! checked by mtime and rebuilt in every new worktree.

#![allow(unused_imports)]

use ::runtime;
#[path = "../../shared-test-support/mod.rs"]
mod shared;

pub use shared::*;
#[allow(dead_code)]
#[path = "../../shared-test-support/restart_fixture.rs"]
mod restart_fixture;
pub use restart_fixture::{fixture_restart_metadata, inherited_fixture_proof};
