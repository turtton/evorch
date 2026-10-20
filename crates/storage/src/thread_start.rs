//! Working-tree state recorded before the first root run of a new thread (ADR 0036 §2).
//!
//! A thread is either replayable, with its complete start content, or not
//! replayable, with only the reason. Partial content is never stored.

/// Default number of days a start state is kept.
pub const THREAD_START_RETENTION_DAYS: u32 = 14;

/// Largest combined size of the diff and untracked contents a replayable state may hold.
pub const MAX_THREAD_START_BYTES: usize = 8 * 1024 * 1024;

/// Why a thread's start state cannot be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotReplayableReason {
    NotGitRepository,
    /// `HEAD` does not name a commit yet.
    NoCommit,
    /// The diff and untracked contents exceed [`MAX_THREAD_START_BYTES`].
    TooLarge,
    /// The secret guard matched the diff or an untracked path or content.
    ContainsSecret,
    TimedOut,
    CaptureFailed,
}

impl NotReplayableReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotGitRepository => "not_git_repository",
            Self::NoCommit => "no_commit",
            Self::TooLarge => "too_large",
            Self::ContainsSecret => "contains_secret",
            Self::TimedOut => "timed_out",
            Self::CaptureFailed => "capture_failed",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        [
            Self::NotGitRepository,
            Self::NoCommit,
            Self::TooLarge,
            Self::ContainsSecret,
            Self::TimedOut,
            Self::CaptureFailed,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackedKind {
    File,
    /// `content` holds the link target.
    Symlink,
}

impl UntrackedKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }

    pub(crate) fn parse(label: &str) -> Option<Self> {
        match label {
            "file" => Some(Self::File),
            "symlink" => Some(Self::Symlink),
            _ => None,
        }
    }
}

/// An untracked, non-ignored entry as it was at the start of the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedEntry {
    /// Path relative to the repository top level, `/`-separated.
    pub path: String,
    pub kind: UntrackedKind,
    pub executable: bool,
    pub content: Vec<u8>,
}

/// Everything needed to rebuild the working tree from `head_commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadStartSnapshot {
    /// The project root relative to the repository top level: empty or ending in `/`.
    pub repo_prefix: String,
    pub head_commit: String,
    /// The full ref `HEAD` named, such as `refs/heads/main`; `None` when detached.
    pub head_ref: Option<String>,
    /// `git diff --binary HEAD` over the whole repository.
    pub diff: Vec<u8>,
    pub untracked: Vec<UntrackedEntry>,
}

impl ThreadStartSnapshot {
    /// Bytes counted against [`MAX_THREAD_START_BYTES`].
    #[must_use]
    pub fn content_bytes(&self) -> usize {
        self.untracked.iter().fold(self.diff.len(), |total, entry| {
            total.saturating_add(entry.content.len())
        })
    }

    fn contains_secret(&self) -> bool {
        let guard = crate::entity::SecretGuard::from_env();
        let leaks = |field, bytes: &[u8]| {
            guard
                .check_text("thread_start", field, &String::from_utf8_lossy(bytes))
                .is_err()
        };
        leaks("diff", &self.diff)
            || self.untracked.iter().any(|entry| {
                leaks("untracked_path", entry.path.as_bytes())
                    || leaks("untracked_content", &entry.content)
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadStartCapture {
    Replayable(ThreadStartSnapshot),
    NotReplayable(NotReplayableReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadStartState {
    pub thread_id: String,
    pub root_run_id: String,
    /// The canonical project root the first run worked in.
    pub project_root: String,
    pub captured_at_ns: i64,
    pub capture: ThreadStartCapture,
}

impl ThreadStartState {
    /// Check the content limit and untracked paths.
    ///
    /// # Errors
    /// Rejects content over [`MAX_THREAD_START_BYTES`], and untracked paths that are
    /// empty, absolute, duplicated or leave the repository.
    pub fn validate(&self) -> Result<(), crate::StorageError> {
        let ThreadStartCapture::Replayable(snapshot) = &self.capture else {
            return Ok(());
        };
        if snapshot.content_bytes() > MAX_THREAD_START_BYTES {
            return Err(crate::StorageError::Serialization(
                "thread start content exceeds 8 MiB".into(),
            ));
        }
        let mut paths = std::collections::HashSet::new();
        for entry in &snapshot.untracked {
            let inside = !entry.path.is_empty()
                && !entry.path.starts_with('/')
                && entry
                    .path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..");
            if !inside || !paths.insert(entry.path.as_str()) {
                return Err(crate::StorageError::Serialization(format!(
                    "invalid thread start untracked path {:?}",
                    entry.path
                )));
            }
        }
        Ok(())
    }

    /// The state as it may be persisted: secret-bearing content becomes
    /// [`NotReplayableReason::ContainsSecret`] without any of that content.
    #[must_use]
    pub(crate) fn without_secrets(mut self) -> Self {
        if let ThreadStartCapture::Replayable(snapshot) = &self.capture
            && snapshot.contains_secret()
        {
            self.capture = ThreadStartCapture::NotReplayable(NotReplayableReason::ContainsSecret);
        }
        self
    }
}

/// A recorded start without its content, for listing threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadStartSummary {
    pub thread_id: String,
    pub root_run_id: String,
    pub project_root: String,
    pub captured_at_ns: i64,
    /// `None` when the thread is replayable.
    pub not_replayable: Option<NotReplayableReason>,
}
