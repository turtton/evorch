//! The thread search's server half: what the core's `search_threads` found, one bounded
//! page at a time, for the project and query the Ctrl+K modal currently holds.
//!
//! This is a state machine without a connection of its own. The workspace feature sends
//! the one request it asks for ([`BodySearch::issue`]) on the window's connection, hands
//! the reply back ([`BodySearch::reply`]) and the modal reads the outcome. Rules:
//!
//! * One request is in flight at a time. A newer `(project, query)` replaces the older
//!   one; the older one's reply is dropped when it arrives, by its epoch.
//! * One *operation* (an input, "Search more", "Search again") sends at most
//!   [`MAX_REQUESTS`] requests, failed ones included. It stops as soon as a page shows a
//!   row the modal did not already list, or when the core has no more pages. A page with
//!   nothing new (no hit, only repeats, or only threads this client cannot open) goes on
//!   to the next page by itself.
//! * A cursor the core no longer accepts (`conflict`) is dropped and the operation starts
//!   again from the first page, once. The rows already shown stay. A second `conflict`, or
//!   no request left, stops the operation until the person asks again.
//! * Nothing is kept for the core: a cursor is the core's own text, passed back unchanged.
use std::collections::BTreeSet;

use workbench_protocol::{
    errors::ErrorCode,
    ids::{ProjectId, ThreadId},
    namespaces::workspace::{
        SearchThreads, ThreadSearchCursor, ThreadSearchHit, ThreadSearchIndex, ThreadSearchResults,
    },
};

use crate::core_client::features::FeatureError;

/// Requests one operation may send, whatever their outcome.
pub const MAX_REQUESTS: u8 = 5;

/// What the modal searches: the project it was opened in and the trimmed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub project: ProjectId,
    pub query: String,
}

/// A thread the core found that this client can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyHit {
    pub thread: ThreadId,
    pub title: Option<String>,
    pub snippet_kind: Option<String>,
    pub snippet: Option<String>,
    pub archived: bool,
}

/// Why an operation stopped before the core ran out of pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The core refused the cursor twice (or had no request left to start over).
    Conflict,
    /// Any other refusal or failure, as the core worded it.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    /// Nothing to search: no project, or a blank query.
    #[default]
    Idle,
    /// The next request is due; it leaves with the next frame.
    Ready,
    /// A request is out.
    Searching,
    /// The operation's rows or budget are used up and the core has more pages.
    Paused,
    /// The core has no more pages.
    Done,
    Stopped(Stop),
}

#[derive(Debug, Default)]
pub struct BodySearch {
    scope: Option<Scope>,
    /// Bumped whenever the scope changes or the connection is reset; a reply of another
    /// epoch is not for what is shown.
    epoch: u64,
    /// The epoch of the request that is out, if one is: only one is, whatever the scope.
    out: Option<u64>,
    hits: Vec<BodyHit>,
    cursor: Option<ThreadSearchCursor>,
    state: State,
    used: u8,
    retried: bool,
    /// The index state the last body page reported. Title pages report none.
    index: Option<ThreadSearchIndex>,
    title_scan_limited: bool,
    /// A reply waiting for the cache, which says which of its threads can be opened.
    pending: Option<Result<ThreadSearchResults, FeatureError>>,
}

/// What the modal draws from the search.
#[derive(Debug, Clone, Copy)]
pub struct SearchView<'a> {
    pub scope: Option<&'a Scope>,
    pub hits: &'a [BodyHit],
    pub state: &'a State,
    pub index: Option<&'a ThreadSearchIndex>,
    pub title_scan_limited: bool,
}

impl SearchView<'_> {
    /// What a window without the workspace feature draws: no search.
    pub fn empty() -> SearchView<'static> {
        static IDLE: State = State::Idle;
        SearchView {
            scope: None,
            hits: &[],
            state: &IDLE,
            index: None,
            title_scan_limited: false,
        }
    }
}

impl BodySearch {
    pub fn view(&self) -> SearchView<'_> {
        SearchView {
            scope: self.scope.as_ref(),
            hits: &self.hits,
            state: &self.state,
            index: self.index.as_ref(),
            title_scan_limited: self.title_scan_limited,
        }
    }

    /// An answer arrived that has not been settled yet.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The modal's current search, `None` when it is closed or has nothing to search. A
    /// changed scope drops everything found for the old one.
    pub fn want(&mut self, scope: Option<Scope>) {
        if self.scope == scope {
            return;
        }
        self.restart(scope);
    }

    /// The connection was reset: whatever was asked on the old one is not coming. The same
    /// scope starts again from its first page.
    pub fn reset(&mut self) {
        self.out = None;
        let scope = self.scope.clone();
        self.restart(scope);
    }

    fn restart(&mut self, scope: Option<Scope>) {
        self.epoch += 1;
        self.state = if scope.is_some() {
            State::Ready
        } else {
            State::Idle
        };
        self.scope = scope;
        self.hits.clear();
        self.cursor = None;
        self.used = 0;
        self.retried = false;
        self.index = None;
        self.title_scan_limited = false;
        self.pending = None;
    }

    /// "Search more": the next page, with a fresh budget.
    pub fn continue_search(&mut self) {
        if self.state == State::Paused {
            self.begin_operation();
        }
    }

    /// "Search again": from the first page, with a fresh budget. The rows shown stay.
    pub fn research(&mut self) {
        if matches!(self.state, State::Paused | State::Stopped(_) | State::Done) {
            self.cursor = None;
            self.begin_operation();
        }
    }

    fn begin_operation(&mut self) {
        self.used = 0;
        self.retried = false;
        self.state = State::Ready;
    }

    /// The request due now, with the epoch to quote when its reply is handed back. At most
    /// one is out at a time.
    pub fn issue(&mut self) -> Option<(u64, SearchThreads)> {
        if self.state != State::Ready || self.out.is_some() || self.pending.is_some() {
            return None;
        }
        let scope = self.scope.as_ref()?;
        let request = SearchThreads {
            project: scope.project.clone(),
            query: scope.query.clone(),
            cursor: self.cursor.clone(),
        };
        self.used += 1;
        self.out = Some(self.epoch);
        self.state = State::Searching;
        Some((self.epoch, request))
    }

    /// The answer to the request issued in `epoch`. An answer of another epoch is dropped.
    pub fn reply(&mut self, epoch: u64, result: Result<ThreadSearchResults, FeatureError>) {
        // The request that was out is over, whatever it was for.
        if self.out == Some(epoch) {
            self.out = None;
        }
        if epoch == self.epoch {
            self.pending = Some(result);
        }
    }

    /// Settles a reply that arrived. `openable` says whether this client can open the
    /// thread (it is in the cache, under the searched project); `listed` holds the
    /// threads the modal lists without the core, by title.
    pub fn settle(&mut self, openable: impl Fn(&ThreadId) -> bool, listed: &BTreeSet<String>) {
        let Some(result) = self.pending.take() else {
            return;
        };
        match result {
            Ok(page) => self.settle_page(page, openable, listed),
            Err(error) => self.settle_error(&error),
        }
    }

    fn settle_page(
        &mut self,
        page: ThreadSearchResults,
        openable: impl Fn(&ThreadId) -> bool,
        listed: &BTreeSet<String>,
    ) {
        let mut new_rows = 0;
        for hit in page.hits.into_iter().filter(|hit| openable(&hit.thread)) {
            match self
                .hits
                .iter_mut()
                .find(|known| known.thread == hit.thread)
            {
                // A repeat: only a snippet it lacked is new, and that is no new row.
                Some(known) => merge_snippet(known, hit),
                None => {
                    if !listed.contains(hit.thread.as_str()) {
                        new_rows += 1;
                    }
                    self.hits.push(BodyHit::from(hit));
                }
            }
        }
        if page.index.is_some() {
            self.index = page.index;
        }
        self.title_scan_limited |= page.title_scan_limited;
        self.cursor = page.next_cursor;
        self.state = if self.cursor.is_none() {
            State::Done
        } else if new_rows > 0 || self.used >= MAX_REQUESTS {
            State::Paused
        } else {
            State::Ready
        };
    }

    fn settle_error(&mut self, error: &FeatureError) {
        self.state = if error.code == Some(ErrorCode::Conflict) {
            if !self.retried && self.used < MAX_REQUESTS {
                // The cursor is gone: start over from the first page, once.
                self.retried = true;
                self.cursor = None;
                State::Ready
            } else {
                State::Stopped(Stop::Conflict)
            }
        } else {
            State::Stopped(Stop::Failed(error.message.clone()))
        };
    }
}

impl From<ThreadSearchHit> for BodyHit {
    fn from(hit: ThreadSearchHit) -> Self {
        Self {
            thread: hit.thread,
            title: hit.title,
            snippet_kind: hit.snippet_kind,
            snippet: hit.snippet,
            archived: hit.archived,
        }
    }
}

fn merge_snippet(known: &mut BodyHit, hit: ThreadSearchHit) {
    if known.snippet.is_none() && hit.snippet.is_some() {
        known.snippet_kind = hit.snippet_kind;
        known.snippet = hit.snippet;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(query: &str) -> Scope {
        Scope {
            project: ProjectId::new("p").unwrap(),
            query: query.into(),
        }
    }

    fn hit(id: &str) -> ThreadSearchHit {
        ThreadSearchHit {
            thread: ThreadId::new(id).unwrap(),
            title: Some(id.into()),
            snippet_kind: Some("user".into()),
            snippet: Some(format!("about {id}")),
            archived: false,
        }
    }

    fn page(ids: &[&str], cursor: Option<&str>) -> ThreadSearchResults {
        ThreadSearchResults {
            hits: ids.iter().map(|id| hit(id)).collect(),
            next_cursor: cursor.map(|c| ThreadSearchCursor::new(c).unwrap()),
            index: None,
            title_scan_limited: false,
        }
    }

    fn conflict() -> FeatureError {
        FeatureError {
            code: Some(ErrorCode::Conflict),
            ..FeatureError::new("cursor gone")
        }
    }

    fn started(query: &str) -> BodySearch {
        let mut search = BodySearch::default();
        search.want(Some(scope(query)));
        search
    }

    /// Issues the due request, answers it and settles the answer; every thread is
    /// openable and none is listed by title.
    fn answer(
        search: &mut BodySearch,
        result: Result<ThreadSearchResults, FeatureError>,
    ) -> Option<Option<ThreadSearchCursor>> {
        let (epoch, request) = search.issue()?;
        search.reply(epoch, result);
        search.settle(|_| true, &BTreeSet::new());
        Some(request.cursor)
    }

    fn requests_until_quiet(
        search: &mut BodySearch,
        mut results: impl FnMut() -> Result<ThreadSearchResults, FeatureError>,
    ) -> u8 {
        let mut sent = 0;
        while *search.view().state == State::Ready {
            answer(search, results()).expect("a request was due");
            sent += 1;
            assert!(sent <= 20, "the operation never stopped");
        }
        sent
    }

    // Given: a core whose pages never hold a row the modal can use
    // When: an operation runs
    // Then: it goes on page by page and stops at the fifth request with the cursor kept
    #[test]
    fn empty_pages_continue_by_themselves_up_to_five_requests() {
        let mut search = started("q");
        let sent = requests_until_quiet(&mut search, || Ok(page(&[], Some("next"))));
        assert_eq!(sent, 5);
        assert_eq!(*search.view().state, State::Paused);
        // "Search more" is a new operation with its own budget, from the kept cursor.
        search.continue_search();
        let (_, request) = search.issue().unwrap();
        assert_eq!(request.cursor.unwrap().as_str(), "next");
    }

    // Given: pages of repeats, then of threads this client cannot open, then a new row
    // When: they arrive
    // Then: only the new row stops the operation, and snippets of repeats are no new row
    #[test]
    fn repeats_and_unopenable_threads_are_no_new_rows() {
        let mut search = started("q");
        answer(&mut search, Ok(page(&["a"], Some("c1"))));
        assert_eq!(*search.view().state, State::Paused);
        search.continue_search();
        let (epoch, _) = search.issue().unwrap();
        search.reply(epoch, Ok(page(&["a", "ghost"], Some("c2"))));
        search.settle(|id| id.as_str() != "ghost", &BTreeSet::new());
        assert_eq!(*search.view().state, State::Ready, "nothing new: goes on");
        let (epoch, _) = search.issue().unwrap();
        search.reply(epoch, Ok(page(&["b"], Some("c3"))));
        search.settle(|id| id.as_str() != "ghost", &BTreeSet::new());
        assert_eq!(*search.view().state, State::Paused);
        let ids: Vec<_> = search
            .view()
            .hits
            .iter()
            .map(|h| h.thread.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    // Given: a thread the modal already lists by its title
    // When: the core finds it too
    // Then: it is not a new row (the operation goes on), but its snippet is kept
    #[test]
    fn a_thread_listed_by_title_is_not_a_new_row() {
        let mut search = started("q");
        let (epoch, _) = search.issue().unwrap();
        search.reply(epoch, Ok(page(&["a"], Some("c1"))));
        let listed = BTreeSet::from(["a".to_owned()]);
        search.settle(|_| true, &listed);
        assert_eq!(*search.view().state, State::Ready);
        assert_eq!(search.view().hits[0].snippet.as_deref(), Some("about a"));
    }

    // Given: the last page
    // When: it carries no cursor
    // Then: the search is done, and nothing is left to continue or send
    #[test]
    fn the_last_page_ends_the_search() {
        let mut search = started("q");
        answer(&mut search, Ok(page(&[], None)));
        assert_eq!(*search.view().state, State::Done);
        assert!(search.issue().is_none());
        search.continue_search();
        assert!(search.issue().is_none());
    }

    // Given: a cursor the core refuses after rows were shown
    // When: the operation goes on
    // Then: it starts over once from the first page keeping the rows, repeats are no new
    //       rows, and the retry counts in the five requests
    #[test]
    fn a_refused_cursor_restarts_once_from_the_first_page_and_counts() {
        let mut search = started("q");
        answer(&mut search, Ok(page(&["a"], Some("c1"))));
        search.continue_search();
        // Requests of this operation: 1 refused, 2 from the start (repeat), 3 empty, 4, 5.
        let mut script = vec![
            Err(conflict()),
            Ok(page(&["a"], Some("d1"))),
            Ok(page(&[], Some("d2"))),
            Ok(page(&[], Some("d3"))),
            Ok(page(&[], Some("d4"))),
        ]
        .into_iter();
        let mut cursors = Vec::new();
        while let Some(cursor) = answer(&mut search, script.next().unwrap_or(Ok(page(&[], None)))) {
            cursors.push(cursor.map(|c| c.as_str().to_owned()));
        }
        assert_eq!(
            cursors,
            [
                Some("c1".into()),
                None,
                Some("d1".into()),
                Some("d2".into()),
                Some("d3".into())
            ]
        );
        assert_eq!(*search.view().state, State::Paused);
        assert_eq!(search.view().hits.len(), 1, "the shown row stayed, once");
    }

    // Given: a core that refuses every cursor
    // When: an operation runs
    // Then: it retries once and then stops, asking the person to search again
    #[test]
    fn a_second_refusal_stops_and_search_again_starts_over() {
        let mut search = started("q");
        answer(&mut search, Ok(page(&["a"], Some("c1"))));
        search.continue_search();
        assert_eq!(requests_until_quiet(&mut search, || Err(conflict())), 2);
        assert_eq!(*search.view().state, State::Stopped(Stop::Conflict));
        search.research();
        let (_, request) = search.issue().unwrap();
        assert!(request.cursor.is_none(), "from the first page");
        assert_eq!(search.view().hits.len(), 1, "rows stay");
    }

    // Given: the retry would be the sixth request
    // When: the fifth request is refused
    // Then: it stops instead of retrying
    #[test]
    fn no_retry_is_sent_beyond_the_budget() {
        let mut search = started("q");
        let mut answers = vec![
            Ok(page(&[], Some("c1"))),
            Ok(page(&[], Some("c2"))),
            Ok(page(&[], Some("c3"))),
            Ok(page(&[], Some("c4"))),
            Err(conflict()),
        ]
        .into_iter();
        let sent = requests_until_quiet(&mut search, || answers.next().unwrap());
        assert_eq!(sent, 5);
        assert_eq!(*search.view().state, State::Stopped(Stop::Conflict));
    }

    // Given: another kind of failure
    // When: it arrives
    // Then: the search stops with the core's words, sends nothing by itself, and "Search
    //       again" tries once more
    #[test]
    fn other_failures_stop_without_resending() {
        let mut search = started("q");
        let sent = requests_until_quiet(&mut search, || Err(FeatureError::new("storage down")));
        assert_eq!(sent, 1);
        assert_eq!(
            *search.view().state,
            State::Stopped(Stop::Failed("storage down".into()))
        );
        search.research();
        assert!(search.issue().is_some());
    }

    // Given: a request in flight for one query
    // When: the query changes, and the old answer arrives
    // Then: the old answer is dropped, nothing is sent while it is out, and the new query
    //       goes out right after
    #[test]
    fn a_late_answer_for_an_older_query_is_dropped() {
        let mut search = started("old");
        let (old_epoch, _) = search.issue().unwrap();
        search.want(Some(scope("new")));
        assert!(search.issue().is_none(), "one request at a time");
        search.reply(old_epoch, Ok(page(&["a"], None)));
        search.settle(|_| true, &BTreeSet::new());
        assert!(search.view().hits.is_empty());
        let (epoch, request) = search.issue().unwrap();
        assert_eq!(request.query, "new");
        search.reply(epoch, Ok(page(&["b"], None)));
        search.settle(|_| true, &BTreeSet::new());
        assert_eq!(search.view().hits[0].thread.as_str(), "b");
    }

    // Given: a request in flight
    // When: the modal closes, the connection resets, or the same scope is asked again
    // Then: closing and resetting drop the answer; asking the same scope changes nothing
    #[test]
    fn closing_and_resetting_drop_what_was_asked() {
        let mut search = started("q");
        let (epoch, _) = search.issue().unwrap();
        search.want(Some(scope("q")));
        assert!(
            search.issue().is_none(),
            "the same scope sends nothing more"
        );
        search.want(None);
        search.reply(epoch, Ok(page(&["a"], None)));
        search.settle(|_| true, &BTreeSet::new());
        assert!(search.view().hits.is_empty());
        assert_eq!(*search.view().state, State::Idle);

        search.want(Some(scope("q")));
        let (epoch, _) = search.issue().unwrap();
        search.reset();
        // The reset forgot the request: the same scope starts again at once.
        let (again, request) = search.issue().unwrap();
        assert!(again > epoch);
        assert!(request.cursor.is_none());
        search.reply(epoch, Ok(page(&["stale"], None)));
        search.settle(|_| true, &BTreeSet::new());
        assert!(search.view().hits.is_empty());
    }

    // Given: a body page that reports the index and a later one that does not
    // When: both arrive
    // Then: the report is kept, as a page without one says nothing about the index
    #[test]
    fn a_page_without_an_index_report_keeps_the_last_report() {
        let mut search = started("q");
        let mut with_index = page(&[], Some("c1"));
        with_index.index = Some(ThreadSearchIndex {
            enabled: Some(true),
            unindexed: 4,
            backfill_in_progress: false,
        });
        answer(&mut search, Ok(with_index));
        answer(&mut search, Ok(page(&[], None)));
        assert_eq!(search.view().index.map(|i| i.unindexed), Some(4));
    }
}
