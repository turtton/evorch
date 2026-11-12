use thiserror::Error;

/// Project registration and allowed-directory boundary errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectError {
    #[error("path must be absolute")]
    NotAbsolute,
    #[error("path is not a directory")]
    NotADirectory,
    #[error("path must be canonical")]
    NotCanonical,
    #[error("failed to canonicalize path '{0}'")]
    Canonicalize(String),
    #[error("project already exists")]
    DuplicateProject,
    #[error("project name must not be empty")]
    EmptyName,
    #[error("allowed directory already exists")]
    DuplicateAllowedDirectory,
    #[error("allowed directory is nested in the project root")]
    NestedInProjectRoot,
    #[error("allowed directory is nested in an existing allowed directory")]
    NestedInExistingAllowed,
    #[error("unknown project")]
    UnknownProject,
    #[error("unknown allowed directory")]
    UnknownAllowedDirectory,
    #[error("allowed directory must not be the filesystem root or the home directory")]
    TooBroad,
}

/// Thread mutation errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThreadError {
    #[error("unknown project")]
    UnknownProject,
    #[error("thread already exists")]
    DuplicateThread,
    #[error("unknown thread")]
    UnknownThread,
    #[error("the thread already belongs to another lane")]
    LaneConflict,
    #[error("invalid extra read directory: {0}")]
    InvalidReadDirectory(String),
    #[error("the thread's family cannot be resolved: {0}")]
    UnresolvedFamily(String),
}

/// Sidebar persistence and validation errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SidebarError {
    #[error("sidebar serialization failed: {0}")]
    Serialization(String),
    #[error("sidebar I/O failed: {0}")]
    Io(String),
    #[error("sidebar validation failed: {0}")]
    Validation(String),
    #[error(transparent)]
    Project(#[from] ProjectError),
}
