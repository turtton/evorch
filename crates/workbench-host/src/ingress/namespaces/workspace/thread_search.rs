//! `search_threads`: the core's thread search for the GUI's search modal.
//!
//! The search reads storage (one bounded page, statement by statement), so it
//! must not run under the host mutex. The host only *prepares* it
//! ([`CommandHost::prepare_thread_search`]: provenance, project resolution, a
//! runtime handle) and the caller runs the owned [`ThreadSearchJob`] after
//! releasing the lock. The job keeps no state between pages: the cursor is the
//! runtime's own text, relayed unchanged.
use runtime::{AgentRuntime, HostThreadHit, HostThreadSearch};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::ids::ThreadId;
use workbench_protocol::limits::MAX_TITLE_BYTES;
use workbench_protocol::namespaces::workspace::{
    MAX_THREAD_SEARCH_KIND_BYTES, MAX_THREAD_SEARCH_SNIPPET_BYTES, THREAD_SEARCH_PAGE_SIZE,
    ThreadSearchCursor, ThreadSearchHit, ThreadSearchIndex, ThreadSearchResults,
};
use workbench_protocol::namespaces::{WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

/// The runtime's continuation texts of the title pages (`host:titles:<n>` and
/// `host:body-start`). Both start with this prefix, and no body cursor does.
/// A page that ends in one has searched titles only. The runtime keeps the
/// constants private; a real-runtime test pins this reading.
const TITLE_PAGE_CURSOR_PREFIX: &str = "host:";

/// One prepared search: owns everything it needs and borrows nothing from the host.
pub struct ThreadSearchJob {
    runtime: AgentRuntime,
    root: std::path::PathBuf,
    query: String,
    cursor: Option<String>,
}

impl CommandHost {
    /// `Some` when `query` is a thread search. Call with the host lock held and
    /// release it before [`ThreadSearchJob::run`]. `provenance` is the
    /// transport's.
    pub fn prepare_thread_search(
        &self,
        query: &Query,
        provenance: &Provenance,
    ) -> Option<Result<ThreadSearchJob, WorkbenchError>> {
        let Query::Workspace(WorkspaceQuery::SearchThreads(search)) = query else {
            return None;
        };
        Some((|| {
            provenance.validate()?;
            let root = self.project_root(&search.project)?;
            Ok(ThreadSearchJob {
                runtime: self.service.runtime().clone(),
                root,
                query: search.query.clone(),
                cursor: search.cursor.as_ref().map(|c| c.as_str().to_owned()),
            })
        })())
    }
}

impl ThreadSearchJob {
    /// Reads one page. Blocking storage I/O, no host lock.
    pub fn run(self) -> Result<QueryReply, WorkbenchError> {
        if !self.runtime.has_run_store() {
            return Err(error(
                ErrorCode::StorageUnavailable,
                "thread search needs the persistent run store",
                RetryAdvice::Never,
            ));
        }
        let search = self
            .runtime
            .search_threads_for_host(
                &self.root,
                &self.query,
                THREAD_SEARCH_PAGE_SIZE,
                self.cursor.as_deref(),
            )
            .map_err(|message| thread_search_error(&message))?;
        let results = results(search)?;
        results.validate()?;
        Ok(QueryReply::Workspace(WorkspaceReply::ThreadSearchResults(
            results,
        )))
    }
}

fn error(code: ErrorCode, message: &str, retry: RetryAdvice) -> WorkbenchError {
    WorkbenchError::new(code, message, retry)
}

/// The one place the runtime's error texts become wire errors. The text is
/// never relayed: it can carry paths, so each class gets a fixed sentence.
pub(crate) fn thread_search_error(message: &str) -> WorkbenchError {
    let starts = |prefix: &str| message.starts_with(prefix);
    if starts("thread_cursor_invalid:")
        || starts("thread_cursor_mismatch:")
        || starts("thread_cursor_stale:")
    {
        error(
            ErrorCode::Conflict,
            "the search cursor is no longer valid; search again from the start",
            RetryAdvice::AfterResync,
        )
    } else if starts("thread_query_invalid:") || starts("thread_pattern_invalid:") {
        error(
            ErrorCode::InvalidRequest,
            "the search query is not acceptable",
            RetryAdvice::Never,
        )
    } else if starts("thread_unavailable:") || starts("thread search needs the persistent") {
        error(
            ErrorCode::StorageUnavailable,
            "the saved threads could not be read",
            RetryAdvice::Never,
        )
    } else if starts("the project folder is not available") {
        error(
            ErrorCode::NotFound,
            "the project folder is not available",
            RetryAdvice::Never,
        )
    } else {
        error(
            ErrorCode::Internal,
            "thread search failed",
            RetryAdvice::Never,
        )
    }
}

fn results(search: HostThreadSearch) -> Result<ThreadSearchResults, WorkbenchError> {
    let redactor = secret_guard::SecretRedactor::from_env();
    let invalid = |what: &str| {
        error(
            ErrorCode::Internal,
            &format!("thread search produced an invalid {what}"),
            RetryAdvice::Never,
        )
    };
    // Titles and body text are displayed, so they are redacted and bounded.
    // Ids and the cursor are not text to display: they are relayed unchanged,
    // or the page is refused.
    let hits = search
        .hits
        .into_iter()
        .map(|hit| hit_view(hit, &redactor).ok_or_else(|| invalid("thread id")))
        .collect::<Result<Vec<_>, _>>()?;
    let titles_only = search
        .next_cursor
        .as_deref()
        .is_some_and(|cursor| cursor.starts_with(TITLE_PAGE_CURSOR_PREFIX));
    let next_cursor = search
        .next_cursor
        .map(|cursor| ThreadSearchCursor::new(cursor).map_err(|_| invalid("cursor")))
        .transpose()?;
    Ok(ThreadSearchResults {
        hits,
        next_cursor,
        index: (!titles_only).then_some(ThreadSearchIndex {
            enabled: search.index.enabled,
            unindexed: search.index.unindexed,
            backfill_in_progress: search.index.backfill_in_progress,
        }),
        title_scan_limited: search.title_scan_limited,
    })
}

fn hit_view(
    hit: HostThreadHit,
    redactor: &secret_guard::SecretRedactor,
) -> Option<ThreadSearchHit> {
    Some(ThreadSearchHit {
        thread: ThreadId::new(hit.thread_id).ok()?,
        title: hit
            .title
            .map(|title| display_text(&title, MAX_TITLE_BYTES, redactor)),
        snippet_kind: hit
            .snippet_kind
            .map(|kind| display_text(&kind, MAX_THREAD_SEARCH_KIND_BYTES, redactor))
            .filter(|kind| !kind.is_empty()),
        snippet: hit
            .snippet
            .map(|snippet| display_text(&snippet, MAX_THREAD_SEARCH_SNIPPET_BYTES, redactor)),
        archived: hit.archived,
    })
}

/// One line without control characters, redacted, then cut at a character
/// boundary at most `max` bytes long.
fn display_text(text: &str, max: usize, redactor: &secret_guard::SecretRedactor) -> String {
    let line = text.replace(char::is_control, " ");
    let redacted = redactor.redact(&line).text.replace(char::is_control, " ");
    let mut end = redacted.len().min(max);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(message: &str) -> ErrorCode {
        thread_search_error(message).code
    }

    #[test]
    fn runtime_errors_map_to_one_wire_code_each() {
        for (message, code) in [
            (
                "thread_cursor_invalid: pass next_cursor unchanged",
                ErrorCode::Conflict,
            ),
            ("thread_cursor_mismatch: other query", ErrorCode::Conflict),
            ("thread_cursor_stale: re-indexed", ErrorCode::Conflict),
            (
                "thread_query_invalid: query terms must not be blank",
                ErrorCode::InvalidRequest,
            ),
            (
                "thread_pattern_invalid: patterns must be 1-8 bytes",
                ErrorCode::InvalidRequest,
            ),
            (
                "thread_unavailable: the thread's saved records could not be read",
                ErrorCode::StorageUnavailable,
            ),
            (
                "thread search needs the persistent run store",
                ErrorCode::StorageUnavailable,
            ),
            ("the project folder is not available", ErrorCode::NotFound),
            (
                "something else at /home/user/secret/path",
                ErrorCode::Internal,
            ),
        ] {
            assert_eq!(code_of(message), code, "{message}");
        }
    }

    #[test]
    fn an_error_never_relays_the_runtime_text() {
        for message in [
            "something else at /home/user/secret/path",
            "thread_unavailable: /home/user/secret/path",
            "thread_cursor_stale: /home/user/secret/path",
        ] {
            let text = thread_search_error(message).message;
            assert!(!text.contains("/home"), "{text}");
        }
    }

    #[test]
    fn display_text_is_one_redacted_line_cut_on_a_character_boundary() {
        let redactor = secret_guard::SecretRedactor::from_env();
        assert_eq!(display_text("a\nb\u{7}c", 100, &redactor), "a b c");
        let secret = format!("{}{}{}", "s", "k-proj-", "B".repeat(64));
        let out = display_text(&format!("key {secret} end"), 100, &redactor);
        assert!(!out.contains(&"B".repeat(64)), "{out}");
        // 3-byte characters cut at 10 bytes keep 9.
        let cut = display_text(&"界".repeat(10), 10, &redactor);
        assert_eq!(cut, "界".repeat(3));
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;
    use runtime::HostSearchIndex;

    fn hit(id: &str, title: &str, snippet: &str) -> HostThreadHit {
        HostThreadHit {
            thread_id: id.into(),
            title: Some(title.into()),
            snippet_kind: Some("user".into()),
            snippet: Some(snippet.into()),
            archived: false,
            updated_at_ns: 0,
        }
    }

    fn page(hits: Vec<HostThreadHit>, next_cursor: Option<&str>) -> HostThreadSearch {
        HostThreadSearch {
            hits,
            next_cursor: next_cursor.map(str::to_owned),
            index: HostSearchIndex {
                enabled: Some(true),
                unindexed: 3,
                backfill_in_progress: false,
            },
            title_scan_limited: false,
        }
    }

    #[test]
    fn long_unicode_and_control_text_still_validates() {
        let title = format!("題\n{}", "名".repeat(2000));
        let snippet = format!("本文\t{}\u{0}{}", "語".repeat(2000), "x".repeat(10));
        let out = results(page(vec![hit("thr-1", &title, &snippet)], None)).unwrap();
        out.validate().unwrap();
        assert!(out.hits[0].title.as_deref().unwrap().len() <= MAX_TITLE_BYTES);
        assert!(out.hits[0].snippet.as_deref().unwrap().len() <= MAX_THREAD_SEARCH_SNIPPET_BYTES);
        assert!(out.hits[0].title.as_deref().unwrap().starts_with("題 "));
    }

    #[test]
    fn ids_and_cursors_are_relayed_unchanged_or_the_page_is_refused() {
        let ok = results(page(vec![hit("thr-1", "t", "s")], Some("abc123"))).unwrap();
        assert_eq!(ok.next_cursor.unwrap().as_str(), "abc123");
        assert_eq!(ok.hits[0].thread.as_str(), "thr-1");
        for broken in [
            results(page(vec![hit(&"i".repeat(200), "t", "s")], None)),
            results(page(vec![], Some(&"c".repeat(2000)))),
            results(page(vec![], Some("a\nb"))),
        ] {
            assert_eq!(broken.unwrap_err().code, ErrorCode::Internal);
        }
    }

    #[test]
    fn only_a_page_that_searched_bodies_reports_the_index() {
        for (cursor, reports) in [
            (Some("host:titles:20"), false),
            (Some("host:body-start"), false),
            (Some("0123abcd"), true),
            (None, true),
        ] {
            let out = results(page(vec![], cursor)).unwrap();
            assert_eq!(out.index.is_some(), reports, "{cursor:?}");
        }
        let out = results(page(vec![], None)).unwrap();
        assert_eq!(out.index.unwrap().unindexed, 3);
    }
}
