//! Thread body search (`search_threads`): the core's own thread search, the same
//! one the model tool uses, answered one bounded page at a time.
//!
//! The cursor is the runtime's own continuation text, relayed unchanged. The
//! core keeps no cursor state: the client repeats the same `(project, query)`
//! with the cursor it was given, and a cursor the core no longer accepts is
//! answered with `conflict` (search again from the start).
use crate::{
    ids::{ProjectId, ThreadId},
    limits::MAX_TITLE_BYTES,
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};

/// Longest query a search carries.
pub const MAX_THREAD_SEARCH_QUERY_BYTES: usize = 1024;
/// Longest continuation text. The runtime's cursors are far shorter.
pub const MAX_THREAD_SEARCH_CURSOR_BYTES: usize = 1024;
/// Hits per page. Fixed: a search names no page size.
pub const THREAD_SEARCH_PAGE_SIZE: usize = 20;
/// Longest snippet around the first match.
pub const MAX_THREAD_SEARCH_SNIPPET_BYTES: usize = 1024;
/// Longest snippet kind label (`title`, `user`, `working_note`, ...).
pub const MAX_THREAD_SEARCH_KIND_BYTES: usize = 32;

/// Continuation of a thread search: opaque, valid only for the same
/// `(project, query)`. Non-empty, no control characters, at most
/// [`MAX_THREAD_SEARCH_CURSOR_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadSearchCursor(String);

impl ThreadSearchCursor {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        check_line("cursor", &value, MAX_THREAD_SEARCH_CURSOR_BYTES, false)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("cursor", &self.0, MAX_THREAD_SEARCH_CURSOR_BYTES, false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchThreads {
    /// The project whose threads are searched. Other projects are never read.
    pub project: ProjectId,
    /// What to look for. Blank lists the most recent threads.
    pub query: String,
    /// The `next_cursor` of the previous page of the same search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ThreadSearchCursor>,
}

impl SearchThreads {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("query", &self.query, MAX_THREAD_SEARCH_QUERY_BYTES, true)?;
        self.cursor
            .as_ref()
            .map_or(Ok(()), ThreadSearchCursor::validate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadSearchHit {
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Where the first match was found: `title`, `user`, `working_note`,
    /// `final_report` or `conversation`. Absent for a listing without a query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_kind: Option<String>,
    /// Redacted single-line text around the first match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub archived: bool,
}

impl ThreadSearchHit {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(title) = &self.title {
            check_line("title", title, MAX_TITLE_BYTES, true)?;
        }
        if let Some(kind) = &self.snippet_kind {
            check_line("snippet_kind", kind, MAX_THREAD_SEARCH_KIND_BYTES, false)?;
        }
        if let Some(snippet) = &self.snippet {
            check_line("snippet", snippet, MAX_THREAD_SEARCH_SNIPPET_BYTES, true)?;
        }
        Ok(())
    }
}

/// What the body index covered. Present only on a page that searched bodies:
/// a title-only page says nothing about the index, and its absence must not be
/// read as "everything is indexed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadSearchIndex {
    /// Whether body indexing is switched on. Absent when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Runs whose bodies are not searchable yet.
    pub unindexed: u64,
    pub backfill_in_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadSearchResults {
    pub hits: Vec<ThreadSearchHit>,
    /// Present while threads remain unexamined: an empty `hits` with a cursor
    /// is not "no match" yet. Pages can repeat a thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<ThreadSearchCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<ThreadSearchIndex>,
    /// Title matching read only the newest threads of a larger project.
    #[serde(default)]
    pub title_scan_limited: bool,
}

impl ThreadSearchResults {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("hits", self.hits.len(), THREAD_SEARCH_PAGE_SIZE)?;
        self.hits.iter().try_for_each(ThreadSearchHit::validate)?;
        self.next_cursor
            .as_ref()
            .map_or(Ok(()), ThreadSearchCursor::validate)
    }
}
