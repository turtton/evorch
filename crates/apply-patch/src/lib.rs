//! Codex-compatible `apply_patch`: parsing, target resolution, and a synchronous
//! plan/commit application engine.
//!
//! Ported from `openai/codex` `codex-rs/apply-patch`; see `UPSTREAM.md` for the
//! source revision and the intentional differences.

mod apply;
mod error;
mod file_update;
mod parser;
mod seek_sequence;
mod streaming_parser;
mod targets;
mod text_file;
pub mod tool_contract;

pub use apply::{
    Applied, AppliedChange, ApplyError, ChangeKind, CommitFailure, Plan, apply_patch, plan,
};
pub use error::ApplyPatchError;
pub use error::ErrorClass;
pub use error::IoError;
pub use error::ParseError;
pub use error::TargetsError;
pub use parser::Hunk;
pub use parser::UpdateFileChunk;
pub use parser::parse_patch;
pub use streaming_parser::StreamingPatchParser;
pub use targets::PatchTarget;
pub use targets::TargetKind;
pub use targets::targets;
pub use targets::targets_raw;

/// Both the raw PATCH argument to `apply_patch` as well as the PATCH argument
/// parsed into hunks.
#[derive(Debug, PartialEq)]
pub struct ApplyPatchArgs {
    pub patch: String,
    pub hunks: Vec<Hunk>,
    pub environment_id: Option<String>,
}
