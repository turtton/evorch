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
    let reason = match &target.database {
        None => "the trial database is not there".to_owned(),
        Some(path) => match open(path) {
            Ok(database) => match project(&database) {
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

/// Opens a trial database for reading only. A finished trial's file is read as
/// an immutable snapshot, which takes no lock and leaves no `-shm` file next to
/// it; one with a `-wal` or `-journal` beside it holds changes the file alone
/// misses, so it is opened the ordinary read-only way. Either way the file is
/// never created, migrated or written, and one on an older schema is refused.
fn open(path: &std::path::Path) -> Result<storage::Database, storage::StorageError> {
    let regular = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file());
    if !regular {
        return Err(storage::StorageError::Serialization(
            "the database is not a regular file".to_owned(),
        ));
    }
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        std::path::PathBuf::from(name).exists()
    };
    if beside("-wal") || beside("-journal") {
        storage::Database::open_read_only(path)
    } else {
        storage::Database::open_read_only_immutable(path)
    }
}

fn project(database: &storage::Database) -> Result<ReplayData, storage::StorageError> {
    let mut registry = TranscriptRegistry::new();
    let mut parts = MessagePartAssembler::new();
    let mut runs: Vec<ReplayRun> = Vec::new();
    let (mut cursor, mut events, mut truncated) = (0, 0, false);
    'pages: loop {
        let page = database.events_after(cursor, PAGE)?;
        for stored in &page.events {
            let Some(event) = parts.accept(&stored.event) else {
                continue;
            };
            if events >= MAX_EVENTS {
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
        if page.scanned < PAGE || page.last_id <= cursor {
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
