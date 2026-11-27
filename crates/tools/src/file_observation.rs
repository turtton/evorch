//! Internal hook through which the built-in file tools report what they read and
//! changed.
//!
//! The runtime sets a [`FileAccessObserver`] on [`crate::ToolExecutionContext`] to
//! learn which files a run knows the contents of and which files a run changed.
//! It is an execution context only: tool schemas, descriptions, arguments and
//! results never change. Only `read`, `edit`, `write` and `apply_patch` report,
//! and only for work that actually happened; `shell`, `grep`, `git_diff`, MCP
//! tools and edits made outside evorch are never observed.

use std::path::Path;

/// One file operation that actually took place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAccess<'a> {
    /// `read` returned the file (a range of it, or an image) successfully.
    Read(&'a Path),
    /// The tool wrote this path with new contents, or created it.
    Written(&'a Path),
    /// The tool removed this path, or moved it away.
    Removed(&'a Path),
}

/// Receives [`FileAccess`] reports from the tool that performed them.
///
/// Implementations must be cheap and must not block or await: a report is made
/// on the tool's own execution path.
pub trait FileAccessObserver: std::fmt::Debug + Send + Sync {
    fn observe(&self, access: FileAccess<'_>);
}

/// Reports `access` when an observer is set.
pub(crate) fn report(observer: Option<&dyn FileAccessObserver>, access: FileAccess<'_>) {
    if let Some(observer) = observer {
        observer.observe(access);
    }
}
