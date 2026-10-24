//! Threads and the runs that belong to them (ADR 0041 §A-1).
//!
//! `sidebar.json` is the source of thread definitions. The GUI mirrors it with
//! [`StorageHandle::try_upsert_threads`](crate::StorageHandle::try_upsert_threads)
//! or [`StorageHandle::upsert_threads_host`](crate::StorageHandle::upsert_threads_host),
//! and the runtime records the threads it starts or branches with
//! [`StorageHandle::upsert_thread_runtime`](crate::StorageHandle::upsert_thread_runtime).
//! Both write the same rows column by column: a value the host wrote is never
//! replaced by the runtime, and `None` never erases a stored value.
//!
//! A run belongs to a thread through `thread_runs`, which saving a run context
//! keeps current. Runs saved before that table existed are still resolved by
//! their name (`chat:<Role>:<thread>`) and parent links.
//!
//! Stored text was redacted or rejected at ingress (run contexts and titles are
//! redacted, notes are guarded); nothing here un-redacts. Callers that show
//! another thread's content must still pass their output through
//! `secret_guard::SecretRedactor`.

use crate::{Database, RunContextRecord, RunLedgerEntry, StorageError};

/// Largest page [`Database::threads_in_project`] returns.
pub const MAX_THREAD_PAGE: usize = 200;

/// Parent links followed before a run's ancestry is treated as broken.
pub const MAX_RUN_LINKS: u32 = 64;

/// How a thread branched from its parent thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadLineageKind {
    /// A separate conversation shown as a child thread.
    Fork,
    /// A replacement version of the parent conversation.
    Rewind,
    /// An orchestrator conversation a worker run requested.
    Escalation,
}

impl ThreadLineageKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Rewind => "rewind",
            Self::Escalation => "escalation",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        [Self::Fork, Self::Rewind, Self::Escalation]
            .into_iter()
            .find(|kind| kind.as_str() == label)
    }
}

/// Who last owned a thread row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadSource {
    /// Written only by the runtime (or inferred, then confirmed by it).
    Runtime,
    /// Mirrored from `sidebar.json`; its values win over the runtime's.
    Host,
    /// Inferred from older run metadata by the v24 migration.
    Backfill,
}

impl ThreadSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Host => "host",
            Self::Backfill => "backfill",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        [Self::Runtime, Self::Host, Self::Backfill]
            .into_iter()
            .find(|source| source.as_str() == label)
    }
}

/// Which writer an upsert speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadWriter {
    Runtime,
    Host,
}

/// A thread definition to write. `None` means "unknown here": it never erases a
/// stored value.
///
/// The host (GUI) replaces every column it supplies. The runtime fills only
/// columns the host left empty, and replaces its own or backfilled values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadUpsert {
    /// Must not be empty.
    pub thread_id: String,
    /// GUI project ID, for display and completion only; never a visibility key.
    pub project_id: Option<String>,
    /// Canonical project directory, compared exactly; the key of project visibility.
    pub project_root: Option<String>,
    /// Redacted before it is stored.
    pub title: Option<String>,
    /// Current conversation root.
    pub root_run_id: Option<String>,
    /// Role name as used in run names (`Role::name()`, such as `Worker`).
    pub chat_role: Option<String>,
    pub parent_thread_id: Option<String>,
    pub lineage_kind: Option<ThreadLineageKind>,
    /// Run whose completed turn the thread branched from.
    pub fork_point_run_id: Option<String>,
    /// Non-system message count of that turn.
    pub fork_point_context_len: Option<u64>,
    /// Worker run that requested this escalated conversation.
    pub escalation_source_run_id: Option<String>,
    /// Stored as `false` when a new row leaves it unknown.
    pub archived: Option<bool>,
    /// Stored as `false` when a new row leaves it unknown.
    pub superseded: Option<bool>,
    /// Unix nanoseconds. The host's positive value replaces the stored one;
    /// the runtime's is used only when none is stored.
    pub created_at_ns: i64,
    /// Unix nanoseconds of the latest activity; the stored value only moves forward.
    pub updated_at_ns: i64,
}

impl ThreadUpsert {
    pub(crate) fn validate(&self) -> Result<(), StorageError> {
        if self.thread_id.is_empty() {
            return Err(StorageError::Serialization("thread id is empty".into()));
        }
        if let Some(length) = self.fork_point_context_len {
            i64::try_from(length).map_err(|_| StorageError::OutOfRange("fork point length"))?;
        }
        Ok(())
    }
}

/// A stored thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub thread_id: String,
    pub project_id: Option<String>,
    /// `None` when unknown: such a thread is never visible by project.
    pub project_root: Option<String>,
    pub title: Option<String>,
    pub root_run_id: Option<String>,
    pub chat_role: Option<String>,
    pub parent_thread_id: Option<String>,
    pub lineage_kind: Option<ThreadLineageKind>,
    pub fork_point_run_id: Option<String>,
    pub fork_point_context_len: Option<u64>,
    pub escalation_source_run_id: Option<String>,
    pub archived: bool,
    pub superseded: bool,
    pub created_at_ns: i64,
    pub updated_at_ns: i64,
    pub source: ThreadSource,
}

/// A run's place in a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRunRow {
    pub run_id: String,
    pub thread_id: String,
    /// The conversation root the run descends from.
    pub root_run_id: String,
    /// Parent links from `root_run_id`; a root is 0.
    pub depth: u32,
}

/// Position after the last thread of a page, ordered by `updated_at_ns` then
/// `thread_id`, both descending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadCursor {
    pub updated_at_ns: i64,
    pub thread_id: String,
}

impl ThreadCursor {
    /// Opaque text form, `<updated_at_ns>:<thread_id>`.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}:{}", self.updated_at_ns, self.thread_id)
    }

    /// Parse [`Self::encode`]'s form; `None` for anything else.
    #[must_use]
    pub fn decode(token: &str) -> Option<Self> {
        let (updated_at_ns, thread_id) = token.split_once(':')?;
        Some(Self {
            updated_at_ns: updated_at_ns.parse().ok()?,
            thread_id: thread_id.to_owned(),
        })
    }
}

/// One page of threads and where the next one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPage {
    pub threads: Vec<ThreadRow>,
    /// `None` on the last page.
    pub next_cursor: Option<ThreadCursor>,
}

/// A run's terminal state, read without returning its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTerminalSummary {
    pub run_id: String,
    pub role: String,
    pub name: String,
    pub parent_run_id: Option<String>,
    /// Phase at the last snapshot, such as `Done`, `Error`, `Stopped` or `Checkpoint`.
    pub terminal_phase: String,
    pub restorable: bool,
    /// Why the run cannot be restored, from its saved configuration.
    pub non_restorable_reason: Option<String>,
    pub updated_at_ns: i64,
    /// Stored messages, system messages included.
    pub message_count: u64,
    /// Text blocks of the last message when the assistant wrote it; for a run
    /// that finished normally this is its published final result.
    pub final_text: Option<String>,
}

/// The `(role, thread)` a conversation root's name `chat:<Role>:<thread>` names.
/// The thread is everything after the role and may itself contain `:`.
#[must_use]
pub fn parse_chat_run_name(name: &str) -> Option<(&str, &str)> {
    let (role, thread) = name.strip_prefix("chat:")?.split_once(':')?;
    (!role.is_empty() && !thread.is_empty()).then_some((role, thread))
}

impl Database {
    /// One thread.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn thread(&self, thread_id: &str) -> Result<Option<ThreadRow>, StorageError> {
        crate::repo::threads::get(&self.conn, thread_id)
    }

    /// Threads whose canonical project root is exactly `project_root`, most
    /// recently updated first. Threads with an unknown project never match.
    /// `limit` is clamped to `1..=MAX_THREAD_PAGE`.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn threads_in_project(
        &self,
        project_root: &str,
        cursor: Option<&ThreadCursor>,
        limit: usize,
    ) -> Result<ThreadPage, StorageError> {
        crate::repo::threads::in_project(&self.conn, project_root, cursor, limit)
    }

    /// Encode a position without exposing a potentially secret-bearing thread ID.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails or the thread was removed.
    pub fn thread_cursor_token(&self, cursor: &ThreadCursor) -> Result<String, StorageError> {
        let rowid: i64 = self.conn.query_row(
            "SELECT rowid FROM threads WHERE thread_id = ?1",
            [&cursor.thread_id],
            |row| row.get(0),
        )?;
        Ok(format!("v1:{}:{rowid}", cursor.updated_at_ns))
    }

    /// Resolve an opaque position. This grants no visibility to its thread.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn thread_cursor_from_token(
        &self,
        token: &str,
    ) -> Result<Option<ThreadCursor>, StorageError> {
        use rusqlite::OptionalExtension;
        let Some((time, rowid)) = token.strip_prefix("v1:").and_then(|s| s.split_once(':')) else {
            return Ok(None);
        };
        let (Ok(updated_at_ns), Ok(rowid)) = (time.parse::<i64>(), rowid.parse::<i64>()) else {
            return Ok(None);
        };
        let thread_id = self
            .conn
            .query_row(
                "SELECT thread_id FROM threads WHERE rowid = ?1",
                [rowid],
                |row| row.get(0),
            )
            .optional()?;
        Ok(thread_id.map(|thread_id| ThreadCursor {
            updated_at_ns,
            thread_id,
        }))
    }

    /// Threads branched from `thread_id`, and escalations requested by its
    /// runs, oldest first.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn thread_children(&self, thread_id: &str) -> Result<Vec<ThreadRow>, StorageError> {
        crate::repo::threads::children(&self.conn, thread_id)
    }

    /// The thread a run belongs to: its own or nearest ancestor's binding in
    /// `thread_runs`, else a parentless ancestor named `chat:<Role>:<thread>` or
    /// recorded as a thread's root. `None` when no ancestor names a thread.
    ///
    /// # Errors
    /// Fails closed on parent cycles or chains longer than [`MAX_RUN_LINKS`],
    /// and on SQLite or decoding errors.
    pub fn thread_of_run(&self, run_id: &str) -> Result<Option<ThreadRunRow>, StorageError> {
        crate::repo::threads::thread_of_run(&self.conn, run_id)
    }

    /// Every run of a thread: its bound runs and conversation roots, and the
    /// runs delegated from them, ordered by depth then run ID.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn runs_of_thread(&self, thread_id: &str) -> Result<Vec<ThreadRunRow>, StorageError> {
        crate::repo::threads::runs_of_thread(&self.conn, thread_id)
    }

    /// Saved contexts of `run_ids` in the given order, without duplicates;
    /// unsaved runs are skipped. Histories can be large: read only what is needed.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn run_contexts_for(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<RunContextRecord>, StorageError> {
        crate::repo::threads::run_contexts_for(&self.conn, run_ids)
    }

    /// The latest note revision of each of `run_ids` that has one, in sequence
    /// order. An empty body is an explicit clear.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn latest_ledgers(&self, run_ids: &[String]) -> Result<Vec<RunLedgerEntry>, StorageError> {
        crate::repo::threads::latest_ledgers(&self.conn, run_ids)
    }

    /// Terminal state of each saved run of `run_ids`, in the given order without
    /// duplicates. Only the last message is decoded.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn terminal_summaries(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<RunTerminalSummary>, StorageError> {
        crate::repo::threads::terminal_summaries(&self.conn, run_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_run_names_split_role_from_a_thread_that_may_contain_colons() {
        assert_eq!(
            parse_chat_run_name("chat:Worker:thread-1"),
            Some(("Worker", "thread-1"))
        );
        assert_eq!(
            parse_chat_run_name("chat:Advisor:thread:consult"),
            Some(("Advisor", "thread:consult"))
        );
        for name in [
            "chat:Worker",
            "chat::thread",
            "chat:Worker:",
            "worker",
            "CHAT:Worker:t",
        ] {
            assert_eq!(parse_chat_run_name(name), None, "{name}");
        }
    }

    #[test]
    fn cursor_round_trips_and_rejects_malformed_tokens() {
        let cursor = ThreadCursor {
            updated_at_ns: 42,
            thread_id: "thread:consult".into(),
        };
        assert_eq!(ThreadCursor::decode(&cursor.encode()), Some(cursor));
        for token in ["", "42", "x:thread", ":thread"] {
            assert_eq!(ThreadCursor::decode(token), None, "{token}");
        }
    }

    #[test]
    fn labels_round_trip() {
        for kind in [
            ThreadLineageKind::Fork,
            ThreadLineageKind::Rewind,
            ThreadLineageKind::Escalation,
        ] {
            assert_eq!(ThreadLineageKind::parse(kind.as_str()), Some(kind));
        }
        for source in [
            ThreadSource::Runtime,
            ThreadSource::Host,
            ThreadSource::Backfill,
        ] {
            assert_eq!(ThreadSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(ThreadLineageKind::parse("Fork"), None);
        assert_eq!(ThreadSource::parse("gui"), None);
    }
}
