//! Error types for parsing patches, resolving their targets, and applying them.

use std::path::PathBuf;

use thiserror::Error;

/// A patch that does not follow the `apply_patch` grammar.
///
/// Ported from upstream `parser.rs`; the variants and messages match upstream.
#[derive(Debug, PartialEq, Error, Clone)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}

/// A patch whose target paths cannot be resolved.
#[derive(Debug, PartialEq, Error, Clone)]
pub enum TargetsError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// A relative path appeared, but no root directory was given to resolve it.
    #[error("relative path '{}' cannot be resolved without a root directory", .0.display())]
    RelativeWithoutRoot(PathBuf),
    /// A path contains a `..` component.
    #[error("path '{}' must not contain '..' components", .0.display())]
    ParentDir(PathBuf),
}

/// A failure while applying a patch.
///
/// Ported from upstream `lib.rs` without the `PathUri` and
/// `ImplicitInvocation` variants, which have no counterpart here. The apply
/// engine extends this as needed.
#[derive(Debug, Error, PartialEq)]
pub enum ApplyPatchError {
    #[error("{}", parse_message(.0))]
    ParseError(#[from] ParseError),
    #[error(transparent)]
    IoError(#[from] IoError),
    /// Error that occurs while computing replacements when applying patch chunks
    #[error("{0}")]
    ComputeReplacements(String),
    #[error(transparent)]
    Path(#[from] TargetsError),
    #[error("No files were modified.")]
    NoFilesModified,
}

/// Coarse classification used by the executor's escalation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Parse,
    Match,
    Io,
    Path,
}

impl ApplyPatchError {
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::ParseError(_) | Self::NoFilesModified => ErrorClass::Parse,
            Self::ComputeReplacements(_) => ErrorClass::Match,
            Self::IoError(_) => ErrorClass::Io,
            Self::Path(TargetsError::Parse(_)) => ErrorClass::Parse,
            Self::Path(_) => ErrorClass::Path,
        }
    }
}

fn parse_message(error: &ParseError) -> String {
    match error {
        ParseError::InvalidPatchError(message) => format!("Invalid patch: {message}"),
        ParseError::InvalidHunkError {
            message,
            line_number,
        } => {
            format!("Invalid patch hunk on line {line_number}: {message}")
        }
    }
}

impl From<std::io::Error> for ApplyPatchError {
    fn from(err: std::io::Error) -> Self {
        ApplyPatchError::IoError(IoError {
            context: "I/O error".to_string(),
            source: err,
        })
    }
}

impl From<&std::io::Error> for ApplyPatchError {
    fn from(err: &std::io::Error) -> Self {
        ApplyPatchError::IoError(IoError {
            context: "I/O error".to_string(),
            source: std::io::Error::new(err.kind(), err.to_string()),
        })
    }
}

/// An I/O failure together with what was being attempted.
#[derive(Debug, Error)]
#[error("{context}: {source}")]
pub struct IoError {
    pub(crate) context: String,
    #[source]
    pub(crate) source: std::io::Error,
}

impl PartialEq for IoError {
    fn eq(&self, other: &Self) -> bool {
        self.context == other.context && self.source.to_string() == other.source.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_messages_match_upstream() {
        assert_eq!(
            ParseError::InvalidPatchError(
                "The first line of the patch must be '*** Begin Patch'".to_string()
            )
            .to_string(),
            "invalid patch: The first line of the patch must be '*** Begin Patch'"
        );
        assert_eq!(
            ParseError::InvalidHunkError {
                message: "Update hunk does not contain any lines".to_string(),
                line_number: 4,
            }
            .to_string(),
            "invalid hunk at line 4, Update hunk does not contain any lines"
        );
    }

    #[test]
    fn targets_error_messages_name_the_path() {
        let parse = ParseError::InvalidPatchError("bad".to_string());
        assert_eq!(
            TargetsError::from(parse.clone()).to_string(),
            parse.to_string()
        );
        assert_eq!(
            TargetsError::RelativeWithoutRoot(PathBuf::from("src/lib.rs")).to_string(),
            "relative path 'src/lib.rs' cannot be resolved without a root directory"
        );
        assert_eq!(
            TargetsError::ParentDir(PathBuf::from("../secret")).to_string(),
            "path '../secret' must not contain '..' components"
        );
    }

    #[test]
    fn apply_patch_error_wraps_io_errors_with_context() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        assert_eq!(ApplyPatchError::from(&io).to_string(), "I/O error: missing");
        assert_eq!(ApplyPatchError::from(&io), ApplyPatchError::from(io));
        assert_eq!(
            ApplyPatchError::from(ParseError::InvalidPatchError("bad".to_string())).to_string(),
            "Invalid patch: bad"
        );
    }
}
