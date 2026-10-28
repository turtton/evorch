//! State of the thread search modal: the query, the selection and the
//! background search it drives.
//!
//! At most one search request is in flight. Typing only changes
//! [`ThreadSearchModal::query`]; once the running request finishes, the next
//! [`ThreadSearchModal::poll`] searches for the newest query. An answer is
//! adopted only if it still answers the current query and project, and results
//! of an older query cannot be confirmed. A search examines a bounded number
//! of threads per request, so the modal keeps the cursor and continues in the
//! background until it has enough matches or has examined every thread; an
//! empty list is "no matches" only after that. Only matches the sidebar can
//! open are kept and counted, so unopenable ones never end the search early.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};

use runtime::{HostSearchIndex, HostThreadHit, HostThreadSearch};
use workspace_ui::{SidebarState, ThreadId};

use super::commands::CommandSink;

/// Matches the modal collects before it stops continuing the search.
pub const SEARCH_LIMIT: usize = 20;

pub type ThreadSearchReceiver = Receiver<Result<HostThreadSearch, String>>;

pub const UNAVAILABLE: &str = "Thread search is unavailable in this session.";
pub const NO_PROJECT: &str = "Select a project to search its threads.";
const INTERRUPTED: &str = "The thread search was interrupted.";

/// What a search answers: the trimmed query within one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchKey {
    pub project: Option<PathBuf>,
    pub query: String,
}

/// Matches collected for one key, across the requests made so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub key: SearchKey,
    pub result: Result<SearchFindings, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchFindings {
    pub hits: Vec<HostThreadHit>,
    pub index: HostSearchIndex,
    /// Where the search continues; `None` once every thread was examined.
    pub next_cursor: Option<String>,
    /// Fuzzy title matching did not reach the project's oldest threads.
    pub title_scan_limited: bool,
}

/// One selectable result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRow {
    pub thread: ThreadId,
    pub title: String,
    /// Where the snippet came from, for example `conversation`.
    pub kind: Option<String>,
    pub snippet: Option<String>,
    pub archived: bool,
}

struct InFlight {
    generation: u64,
    key: SearchKey,
    /// The cursor this request continued from; `None` for a first request.
    from: Option<String>,
    receiver: ThreadSearchReceiver,
}

/// Keys pressed this frame that act on the search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchKeys {
    pub down: bool,
    pub up: bool,
    pub enter: bool,
    pub escape: bool,
}

#[derive(Default)]
pub struct ThreadSearchModal {
    open: bool,
    pub query: String,
    selected: usize,
    /// Bumped on open and close, so an answer to an earlier session is dropped.
    generation: u64,
    in_flight: Option<InFlight>,
    outcome: Option<SearchOutcome>,
    /// Focus the query field on the next frame.
    focus_pending: bool,
    keys: SearchKeys,
    /// An IME conversion is open in the query field.
    composing: bool,
    /// The layer the modal was drawn on last frame, to tell when another modal is above it.
    layer: Option<egui::LayerId>,
}

impl ThreadSearchModal {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.close();
        self.open = true;
        self.focus_pending = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.generation += 1;
        self.query.clear();
        self.selected = 0;
        self.in_flight = None;
        self.outcome = None;
        self.keys = SearchKeys::default();
        self.composing = false;
        self.layer = None;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_pending)
    }

    /// Records the layer the modal was drawn on.
    pub fn note_layer(&mut self, layer: egui::LayerId) {
        self.layer = Some(layer);
    }

    /// Takes this modal's keys out of the frame's input before any pane sees
    /// them, so Esc, Enter and the arrows never reach the composer, a terminal
    /// or another pane behind the modal. Keys that belong to an IME conversion
    /// are removed as well but do not act on the search, and while another
    /// modal is above the search the keys are left to it.
    pub fn capture_keys(&mut self, ctx: &egui::Context) {
        use egui::{Event, ImeEvent, Key, Modifiers};
        if !self.open {
            return;
        }
        if self.layer.is_some() && ctx.memory(|memory| memory.top_modal_layer()) != self.layer {
            return;
        }
        let (mut composing, mut ime_event) = (self.composing, false);
        ctx.input(|input| {
            for event in &input.events {
                if let Event::Ime(event) = event {
                    ime_event = true;
                    #[allow(deprecated)] // Accept legacy backend Enabled/Disabled events too.
                    match event {
                        ImeEvent::Preedit { text, .. } => composing = !text.is_empty(),
                        ImeEvent::Commit(_) | ImeEvent::Disabled => composing = false,
                        ImeEvent::Enabled | ImeEvent::DeleteSurrounding { .. } => {}
                    }
                }
            }
        });
        // The key that confirms or cancels a conversion arrives with its IME
        // event (or while the conversion is open) and is not a search command.
        let for_ime = self.composing || ime_event;
        self.composing = composing;
        ctx.input_mut(|input| {
            let down = input.consume_key(Modifiers::NONE, Key::ArrowDown);
            let up = input.consume_key(Modifiers::NONE, Key::ArrowUp);
            let enter = input.consume_key(Modifiers::NONE, Key::Enter);
            let escape = input.consume_key(Modifiers::NONE, Key::Escape);
            if !for_ime {
                self.keys.down |= down;
                self.keys.up |= up;
                self.keys.enter |= enter;
                self.keys.escape |= escape;
            }
        });
    }

    pub fn take_keys(&mut self) -> SearchKeys {
        std::mem::take(&mut self.keys)
    }

    pub fn outcome(&self) -> Option<&SearchOutcome> {
        self.outcome.as_ref()
    }

    pub fn key(&self, project: Option<&Path>) -> SearchKey {
        SearchKey {
            project: project.map(Path::to_path_buf),
            query: self.query.trim().to_owned(),
        }
    }

    /// Whether the shown results answer something other than the current
    /// query and project. Such results may not be confirmed.
    pub fn is_stale(&self, project: Option<&Path>) -> bool {
        self.outcome
            .as_ref()
            .is_none_or(|outcome| outcome.key != self.key(project))
    }

    /// Whether the search for the current query is still looking.
    pub fn is_searching(&self, project: Option<&Path>) -> bool {
        self.is_stale(project)
            || self.in_flight.is_some()
            || self.outcome.as_ref().is_some_and(|outcome| {
                outcome
                    .result
                    .as_ref()
                    .is_ok_and(|found| found.next_cursor.is_some() && !found.is_full())
            })
    }

    /// Collects a finished request and starts the one the current query needs:
    /// the first request for a new query, or the continuation of the current
    /// one while it has fewer matches than [`SEARCH_LIMIT`].
    /// Returns whether a request is still running, so the caller can poll again.
    pub fn poll(
        &mut self,
        sink: &dyn CommandSink,
        project: Option<&Path>,
        sidebar: &SidebarState,
    ) -> bool {
        let current = self.key(project);
        if let Some(running) = &self.in_flight {
            match running.receiver.try_recv() {
                Err(TryRecvError::Empty) => return true,
                Ok(answer) => {
                    let adopt = running.generation == self.generation && running.key == current;
                    if adopt {
                        self.adopt(running.key.clone(), running.from.is_some(), answer, sidebar);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    if running.generation == self.generation && running.key == current {
                        self.adopt(running.key.clone(), false, Err(INTERRUPTED.into()), sidebar);
                    }
                }
            }
            self.in_flight = None;
        }
        if !self.open {
            return false;
        }
        let from = match &self.outcome {
            Some(outcome) if outcome.key == current => match &outcome.result {
                Ok(found) if !found.is_full() => match &found.next_cursor {
                    Some(cursor) => Some(cursor.clone()),
                    None => return false,
                },
                _ => return false,
            },
            _ => None,
        };
        let started = match &current.project {
            None => Err(NO_PROJECT),
            Some(project) => sink
                .search_threads(
                    project.clone(),
                    current.query.clone(),
                    SEARCH_LIMIT.saturating_sub(self.hit_count()).max(1),
                    from.clone(),
                )
                .ok_or(UNAVAILABLE),
        };
        match started {
            Ok(receiver) => {
                self.in_flight = Some(InFlight {
                    generation: self.generation,
                    key: current,
                    from,
                    receiver,
                });
                true
            }
            Err(reason) => {
                self.outcome = Some(SearchOutcome {
                    key: current,
                    result: Err(reason.into()),
                });
                false
            }
        }
    }

    fn hit_count(&self) -> usize {
        self.outcome
            .as_ref()
            .and_then(|outcome| outcome.result.as_ref().ok())
            .map_or(0, |found| found.hits.len())
    }

    /// Keeps the matches the sidebar can open and merges them into the
    /// results for `key`, or starts those results over for a first request.
    fn adopt(
        &mut self,
        key: SearchKey,
        continuation: bool,
        answer: Result<HostThreadSearch, String>,
        sidebar: &SidebarState,
    ) {
        let openable = |hit: &HostThreadHit| {
            sidebar
                .threads
                .iter()
                .any(|thread| thread.id.to_string() == hit.thread_id)
        };
        let page = match answer {
            Ok(page) => page,
            Err(error) => {
                self.outcome = Some(SearchOutcome {
                    key,
                    result: Err(error),
                });
                return;
            }
        };
        let mut found = match self.outcome.take() {
            Some(SearchOutcome {
                key: previous,
                result: Ok(found),
            }) if continuation && previous == key => found,
            _ => SearchFindings::default(),
        };
        for hit in page.hits.into_iter().filter(openable) {
            if !found
                .hits
                .iter()
                .any(|seen| seen.thread_id == hit.thread_id)
            {
                found.hits.push(hit);
            }
        }
        found.index = page.index;
        found.next_cursor = page.next_cursor;
        found.title_scan_limited |= page.title_scan_limited;
        self.outcome = Some(SearchOutcome {
            key,
            result: Ok(found),
        });
    }

    /// The shown results that the sidebar can open, best first. A thread the
    /// sidebar no longer lists (for example a hidden rewind version) is skipped.
    pub fn rows(&self, sidebar: &SidebarState) -> Vec<ResultRow> {
        let Some(Ok(found)) = self.outcome.as_ref().map(|outcome| &outcome.result) else {
            return Vec::new();
        };
        found
            .hits
            .iter()
            .filter_map(|hit| {
                let record = sidebar
                    .threads
                    .iter()
                    .find(|thread| thread.id.to_string() == hit.thread_id)?;
                Some(ResultRow {
                    thread: record.id.clone(),
                    title: record.title.clone(),
                    kind: hit.snippet_kind.clone(),
                    snippet: hit.snippet.clone(),
                    archived: record.archived,
                })
            })
            .collect()
    }

    pub fn selected(&self, rows: usize) -> usize {
        self.selected.min(rows.saturating_sub(1))
    }

    pub fn move_selection(&mut self, rows: usize, delta: isize) {
        if rows == 0 {
            self.selected = 0;
            return;
        }
        let last = rows - 1;
        let current = self.selected(rows);
        self.selected = if delta < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(last)
        };
    }

    /// Typing restarts the selection at the best match.
    pub fn reset_selection(&mut self) {
        self.selected = 0;
    }
}

impl SearchFindings {
    /// Enough matches were found for the modal to stop continuing.
    pub fn is_full(&self) -> bool {
        self.hits.len() >= SEARCH_LIMIT
    }
}
