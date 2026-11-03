//! Reading back the conversation of a finished evaluation trial.
//!
//! A trial keeps its own database (`evorch.db`). It is opened read-only, never
//! created, migrated or written, and its events are projected into a
//! [`TranscriptRegistry`] that belongs to this replay alone: nothing here
//! touches the Workbench's transcripts, runs, phases or usage. A database the
//! current schema cannot open read-only (an older one) is reported as such and
//! the trial's `conversation.json` is offered instead.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use eval::conversation::Entry;
use eval::inspection::{Artifact, ArtifactProblem, ProblemKind};
use event_bus::{EventKind, LifecycleEvent, MessagePartAssembler};

use crate::model::transcript::TranscriptModel;
use crate::model::transcript_registry::TranscriptRegistry;

/// Events read per page from the trial database.
const PAGE: usize = 500;
/// Events projected at most; a longer trial shows its beginning.
pub const MAX_EVENTS: usize = 60_000;
/// Rows read at most, whatever became of them: message parts still waiting for
/// their rest and rows this version cannot decode cost a read and project
/// nothing, so they need a bound of their own.
pub const MAX_SCANNED_ROWS: usize = 240_000;

/// How much of a trial database one replay reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub page: usize,
    pub events: usize,
    pub scanned_rows: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            page: PAGE,
            events: MAX_EVENTS,
            scanned_rows: MAX_SCANNED_ROWS,
        }
    }
}
/// Bytes of `conversation.json` read at most.
const CONVERSATION_BYTES: u64 = 2 * 1024 * 1024;

/// Where a trial's records are, taken from the inspection snapshot. A path is
/// given only when the inspection saw a regular file there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayTarget {
    pub database: Option<PathBuf>,
    pub conversation: Option<PathBuf>,
}

/// A run of the replayed trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRun {
    pub run_id: String,
    pub parent: Option<String>,
    pub name: String,
    pub role: String,
}

/// The projection of one trial's events. Read-only once built.
#[derive(Debug)]
pub struct ReplayData {
    registry: TranscriptRegistry,
    runs: Vec<ReplayRun>,
    events: usize,
    truncated: bool,
}

impl ReplayData {
    /// Root runs first in start order, each followed by what it delegated.
    pub fn runs(&self) -> &[ReplayRun] {
        &self.runs
    }

    pub fn transcript(&self, run_id: &str) -> Option<&TranscriptModel> {
        self.registry.run(run_id)
    }

    pub fn events(&self) -> usize {
        self.events
    }

    /// The trial has more events than were projected.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The run to show first: the first one nobody delegated.
    pub fn default_run(&self) -> Option<&str> {
        self.runs
            .iter()
            .find(|run| run.parent.is_none())
            .or_else(|| self.runs.first())
            .map(|run| run.run_id.as_str())
    }

    /// Nesting depth of a run below its root.
    pub fn depth(&self, run: &ReplayRun) -> usize {
        let mut depth = 0;
        let mut parent = run.parent.as_deref();
        while let Some(id) = parent {
            depth += 1;
            if depth > 16 {
                break;
            }
            parent = self
                .runs
                .iter()
                .find(|candidate| candidate.run_id == id)
                .and_then(|candidate| candidate.parent.as_deref());
        }
        depth
    }
}

/// What reading a trial's records gave.
#[derive(Debug, Clone)]
pub enum ReplayLoad {
    Ready(Arc<ReplayData>),
    /// The database could not be used; `reason` says why. `conversation` is
    /// what the user saw, from `conversation.json`.
    Fallback {
        reason: String,
        conversation: Artifact<Vec<Entry>>,
    },
}

pub fn load(target: &ReplayTarget) -> ReplayLoad {
    load_with(target, Limits::default())
}

pub fn load_with(target: &ReplayTarget, limits: Limits) -> ReplayLoad {
    let reason = match &target.database {
        None => "the trial database is not there".to_owned(),
        Some(path) => match open(path) {
            Ok(database) => match project(&database, limits) {
                Ok(data) => return ReplayLoad::Ready(Arc::new(data)),
                Err(error) => format!("cannot read the trial database: {error}"),
            },
            Err(error) => format!("cannot open the trial database: {error}"),
        },
    };
    ReplayLoad::Fallback {
        reason,
        conversation: conversation(target),
    }
}

/// Opens a trial database for reading only, as an immutable snapshot: no lock,
/// no journal and no `-shm` file is made next to it, and the file is never
/// created, migrated or written. An older schema is refused.
///
/// A `-wal` or `-journal` file with content beside the database holds changes
/// the file alone misses, and reading them the ordinary way would create or
/// update files in the evaluation's directory. That is refused too; the
/// conversation file stands in.
fn open(path: &std::path::Path) -> Result<storage::Database, storage::StorageError> {
    let regular = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file());
    if !regular {
        return Err(storage::StorageError::Serialization(
            "the database is not a regular file".to_owned(),
        ));
    }
    for suffix in ["-wal", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        if std::fs::symlink_metadata(&sidecar).is_ok_and(|meta| meta.len() > 0) {
            return Err(storage::StorageError::Serialization(format!(
                "a {suffix} file with content sits next to the database, so the trial did not \
                 close it cleanly and reading it would write beside the evaluation's files"
            )));
        }
    }
    storage::Database::open_read_only_immutable(path)
}

fn project(
    database: &storage::Database,
    limits: Limits,
) -> Result<ReplayData, storage::StorageError> {
    let mut registry = TranscriptRegistry::new();
    let mut parts = MessagePartAssembler::new();
    let mut runs: Vec<ReplayRun> = Vec::new();
    let (mut cursor, mut events, mut scanned, mut truncated) = (0, 0, 0usize, false);
    'pages: loop {
        let page = database.events_after(cursor, limits.page)?;
        for stored in &page.events {
            let Some(event) = parts.accept(&stored.event) else {
                continue;
            };
            if events >= limits.events {
                truncated = true;
                break 'pages;
            }
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                agent_name,
                role,
            }) = &event.kind
                && !runs.iter().any(|run| &run.run_id == run_id)
            {
                runs.push(ReplayRun {
                    run_id: run_id.clone(),
                    parent: parent_run_id.clone(),
                    name: agent_name.clone(),
                    role: role.clone(),
                });
            }
            registry.apply(&event);
            events += 1;
        }
        scanned += page.scanned;
        if page.scanned < limits.page || page.last_id <= cursor {
            break;
        }
        if scanned >= limits.scanned_rows {
            truncated = database.events_after(page.last_id, 1)?.scanned > 0;
            break;
        }
        cursor = page.last_id;
    }
    registry.finish_history();
    for run_id in registry.run_ids() {
        if !runs.iter().any(|run| &run.run_id == run_id) {
            runs.push(ReplayRun {
                run_id: run_id.clone(),
                parent: None,
                name: run_id.clone(),
                role: String::new(),
            });
        }
    }
    let runs = ordered(runs);
    Ok(ReplayData {
        registry,
        runs,
        events,
        truncated,
    })
}

/// Each root followed by its descendants, in start order.
fn ordered(runs: Vec<ReplayRun>) -> Vec<ReplayRun> {
    fn visit(parent: Option<&str>, runs: &[ReplayRun], out: &mut Vec<ReplayRun>, depth: usize) {
        if depth > 16 {
            return;
        }
        for run in runs.iter().filter(|run| run.parent.as_deref() == parent) {
            if out.iter().any(|seen| seen.run_id == run.run_id) {
                continue;
            }
            out.push(run.clone());
            visit(Some(&run.run_id), runs, out, depth + 1);
        }
    }
    let mut out = Vec::with_capacity(runs.len());
    visit(None, &runs, &mut out, 0);
    // A run whose parent never started in this trial is still listed.
    for run in &runs {
        if !out.iter().any(|seen| seen.run_id == run.run_id) {
            out.push(run.clone());
        }
    }
    out
}

fn conversation(target: &ReplayTarget) -> Artifact<Vec<Entry>> {
    let Some(path) = &target.conversation else {
        return Artifact::Missing;
    };
    let problem =
        |kind, detail: String| Artifact::Problem(ArtifactProblem::new(kind, path, detail));
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Artifact::Missing,
        Err(error) => return problem(ProblemKind::Io, error.to_string()),
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(CONVERSATION_BYTES + 1).read_to_end(&mut bytes) {
        return problem(ProblemKind::Io, error.to_string());
    }
    if bytes.len() as u64 > CONVERSATION_BYTES {
        return problem(
            ProblemKind::TooLarge,
            format!("larger than {CONVERSATION_BYTES} bytes"),
        );
    }
    match serde_json::from_slice(&bytes) {
        Ok(entries) => Artifact::Present(entries),
        Err(error) => problem(ProblemKind::Malformed, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use event_bus::{Event, MessageEvent};
    use storage::{Storage, StorageConfig};

    use super::*;

    fn part(response: usize) -> Event {
        Event::new(MessageEvent::MessagePart {
            run_id: "run-1".into(),
            response_id: format!("response-{response}"),
            text: "first half".into(),
            index: 0,
            total: 2,
        })
    }

    fn answer(text: &str) -> Event {
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: text.into(),
        })
    }

    /// A trial database holding `events`, then `undecodable` rows that this
    /// version cannot read.
    fn trial_database(events: &[Event], undecodable: usize) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("evorch.db");
        let storage = Storage::open(StorageConfig {
            db_path: path.clone(),
            ..StorageConfig::default()
        })
        .expect("trial storage");
        for event in events {
            storage
                .handle()
                .append_event(Some("trial"), event)
                .expect("append");
        }
        drop(storage);
        let raw = rusqlite::Connection::open(&path).expect("raw connection");
        for _ in 0..undecodable {
            raw.execute(
                "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, \
                 kind, payload) VALUES ('trial', 1, 0, 0, 'FromTheFuture', '{\"nothing\":1}')",
                [],
            )
            .expect("insert an unreadable row");
        }
        drop(raw);
        (dir, path)
    }

    fn replay(path: &Path, limits: Limits) -> Arc<ReplayData> {
        let target = ReplayTarget {
            database: Some(path.to_path_buf()),
            conversation: None,
        };
        match load_with(&target, limits) {
            ReplayLoad::Ready(data) => data,
            ReplayLoad::Fallback { reason, .. } => panic!("should replay: {reason}"),
        }
    }

    fn limits(page: usize, scanned_rows: usize) -> Limits {
        Limits {
            page,
            events: MAX_EVENTS,
            scanned_rows,
        }
    }

    #[test]
    fn rows_that_project_nothing_still_spend_the_scan_budget() {
        // Given: eight message parts that never complete, so no event is projected.
        let parts: Vec<_> = (0..8).map(part).collect();
        let (_dir, path) = trial_database(&parts, 0);

        // When: the replay may read six rows, two at a time.
        let data = replay(&path, limits(2, 6));

        // Then: it stops there and says the trial has more.
        assert_eq!(data.events(), 0);
        assert!(data.truncated());
    }

    #[test]
    fn rows_that_cannot_be_decoded_spend_the_scan_budget_too() {
        // Given: one answer followed by unreadable rows.
        let (_dir, path) = trial_database(&[answer("kept")], 9);

        // When: the replay may read four rows, two at a time.
        let data = replay(&path, limits(2, 4));

        // Then: the answer is shown, reading stopped early, and the omission is stated.
        assert_eq!(data.events(), 1);
        assert!(data.truncated());
    }

    #[test]
    fn a_trial_that_ends_exactly_at_the_budget_is_not_called_truncated() {
        // Given: eight rows, a page size that divides them, and a budget of exactly eight.
        let parts: Vec<_> = (0..8).map(part).collect();
        let (_dir, path) = trial_database(&parts, 0);

        // When / Then: reading all of them is not a truncation, whether the last page is full ...
        assert!(!replay(&path, limits(2, 8)).truncated());
        // ... or short.
        assert!(!replay(&path, limits(3, 8)).truncated());
        // And: a budget spent before the end (checked a page at a time) is.
        assert!(replay(&path, limits(2, 5)).truncated());
    }

    #[test]
    fn the_projected_event_limit_still_applies_to_decodable_events() {
        // Given: five answers.
        let answers: Vec<_> = (0..5).map(|n| answer(&format!("answer {n}"))).collect();
        let (_dir, path) = trial_database(&answers, 0);

        // When: at most three events may be projected.
        let data = replay(
            &path,
            Limits {
                page: 2,
                events: 3,
                scanned_rows: MAX_SCANNED_ROWS,
            },
        );

        // Then: three are shown and the rest is reported as omitted.
        assert_eq!(data.events(), 3);
        assert!(data.truncated());
    }
}
