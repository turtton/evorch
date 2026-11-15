//! Host service of the `memory` area, read side (PR 13): the lessons of a
//! project. PR 14 adds the update side to this file, with the identity,
//! revision and action rules below.
//!
//! * Reads are operations of [`read_ops`](super::diagnostics::read_ops): the
//!   handler registers the read and returns at once; the database is opened
//!   read-only on a blocking worker, never through the runtime's shared
//!   connection and never under the host's mutex.
//! * The client names a project; the adapter resolves its directory, and the
//!   lesson project (the repository slug) is resolved on the worker, because
//!   deriving it may run `git`.
//! * Search words are literals: each one is quoted for the full-text index, so
//!   no input is a syntax error or an operator.
//! * A lesson's [`revision`] is the number of its status rows plus the number
//!   of its favorite changes: it grows on every change and only then.
use super::{
    AreaHandles, AreaTasks, ProjectionSource,
    diagnostics::read_ops::{
        ReadOps, SlugOf, Snapshot, bounded, digest_of, first_line, page_of, runtime_slug,
    },
};
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, error, storage_error};
use std::path::{Path, PathBuf};
use storage::{
    Database,
    memory::{MemoryEntry, MemoryFilter, MemoryStatus as StoredStatus},
};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    events::UiEvent,
    ids::MemoryId,
    management::{
        ActionAvailability, ActionState, DenialReason, MemoryAction, MemoryState, MemoryStatus,
    },
    namespaces::{
        diagnostics::ReadOutcome,
        memory::{
            GetMemory, ListMemory, MAX_PREVIEW_BYTES, MemoryDetail, MemoryEvent, MemoryScope,
            MemorySummary,
        },
    },
    operation::OperationView,
    views::Page,
    wire::Revision,
};

/// What a finished read holds, before it is cut into pages.
#[derive(Clone)]
pub enum MemoryRead {
    List(Vec<MemorySummary>),
    Detail(Box<MemoryDetail>),
}

/// The `memory` service, reached from `ingress/namespaces/memory.rs` as
/// `self.areas.memory`. Without handles (the unwired form) every read is refused.
#[derive(Default)]
pub struct MemoryArea {
    reads: ReadOps<MemoryRead>,
    pager: Pager,
    wired: Option<Wired>,
}

#[derive(Clone)]
struct Wired {
    slug: SlugOf,
    database: PathBuf,
}

impl MemoryArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("memory")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::from_parts(
            handles.tasks.clone(),
            source,
            handles.database.clone(),
            runtime_slug(handles.runtime.clone()),
        ))
    }

    /// The service over its parts: the host's tasks, the namespace's event
    /// source, the event database file and the lesson project of a directory.
    pub fn from_parts(
        tasks: AreaTasks,
        source: ProjectionSource,
        database: PathBuf,
        slug: SlugOf,
    ) -> Self {
        Self {
            reads: ReadOps::new(tasks, move |view| {
                let _ = source.publish(UiEvent::Memory(MemoryEvent::MemoryOperationChanged {
                    view,
                }));
            }),
            pager: Pager::default(),
            wired: Some(Wired { slug, database }),
        }
    }

    fn wired(&self) -> Result<&Wired, WorkbenchError> {
        self.wired.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "memory is not available in this host",
            )
        })
    }

    /// Start or join a list of the lessons of the project in `root`.
    ///
    /// # Errors
    /// See [`ReadOps::submit`]; `Conflict` also for a cursor that is not this
    /// operation's.
    pub fn list(
        &self,
        root: PathBuf,
        list: &ListMemory,
    ) -> Result<ReadOutcome<Page<MemorySummary>>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let mut identity = list.clone();
        identity.page = Default::default();
        let filter = MemoryFilter {
            status: list.status.map(stored_status),
            favorite: list.favorite,
        };
        let words = list.text.as_deref().map(literal_words).unwrap_or_default();
        let snapshot = self.reads.submit(
            &list.operation_id,
            digest_of(&("list", &identity)),
            move || {
                let slug = (wired.slug)(&root);
                let database = open(&wired.database)?;
                let entries = database
                    .search_memory_filtered(&slug, &words, filter)
                    .map_err(storage_error)?;
                entries
                    .iter()
                    .map(|entry| summary(&database, entry))
                    .collect::<Result<Vec<_>, _>>()
                    .map(MemoryRead::List)
            },
        )?;
        let data = match snapshot.data {
            Some(MemoryRead::List(items)) => Some(page_of(
                &self.pager,
                &list.operation_id,
                &items,
                &list.page,
            )?),
            _ => None,
        };
        Ok(ReadOutcome {
            operation: snapshot.view,
            data,
        })
    }

    /// Start or join the read of one lesson.
    ///
    /// # Errors
    /// See [`ReadOps::submit`].
    pub fn get(
        &self,
        root: PathBuf,
        get: &GetMemory,
    ) -> Result<ReadOutcome<MemoryDetail>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let id = get.id.as_str().to_owned();
        let Snapshot { view, data } = self.reads.submit(
            &get.operation_id,
            digest_of(&("get", &get.project, &get.id)),
            move || {
                let slug = (wired.slug)(&root);
                let database = open(&wired.database)?;
                let entry = database
                    .memory_entry(&id)
                    .map_err(storage_error)?
                    // A lesson of another project is not this project's to read.
                    .filter(|entry| entry.lesson.project == slug)
                    .ok_or_else(|| error(ErrorCode::NotFound, "unknown lesson"))?;
                Ok(MemoryRead::Detail(Box::new(detail(&database, &entry)?)))
            },
        )?;
        Ok(ReadOutcome {
            operation: view,
            data: match data {
                Some(MemoryRead::Detail(detail)) => Some(*detail),
                _ => None,
            },
        })
    }

    /// The view of an operation of this area.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not know.
    pub fn operation(
        &self,
        id: &workbench_protocol::ids::OperationId,
    ) -> Result<OperationView, WorkbenchError> {
        self.reads.view(id)
    }
}

fn open(path: &Path) -> Result<Database, WorkbenchError> {
    Database::open_read_only(path).map_err(storage_error)
}

fn stored_status(status: MemoryStatus) -> StoredStatus {
    match status {
        MemoryStatus::Candidate => StoredStatus::Candidate,
        MemoryStatus::Validated => StoredStatus::Validated,
        MemoryStatus::Promoted => StoredStatus::Promoted,
        MemoryStatus::Rejected => StoredStatus::Rejected,
    }
}

fn wire_status(status: StoredStatus) -> MemoryStatus {
    match status {
        StoredStatus::Candidate => MemoryStatus::Candidate,
        StoredStatus::Validated => MemoryStatus::Validated,
        StoredStatus::Promoted => MemoryStatus::Promoted,
        StoredStatus::Rejected => MemoryStatus::Rejected,
    }
}

fn wire_scope(scope: storage::memory::LessonScope) -> MemoryScope {
    match scope {
        storage::memory::LessonScope::Project => MemoryScope::Project,
        storage::memory::LessonScope::Harness => MemoryScope::Harness,
        storage::memory::LessonScope::User => MemoryScope::User,
    }
}

/// Every word of `text` as a quoted literal of the full-text index.
fn literal_words(text: &str) -> String {
    text.split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What may be done to a lesson now. PR 14's commands refuse exactly what is
/// denied here.
pub fn actions(status: MemoryStatus, favorite: bool) -> Vec<ActionAvailability<MemoryAction>> {
    let rejected = status == MemoryStatus::Rejected;
    let allowed_if = |ok: bool, otherwise: DenialReason| {
        if ok {
            ActionState::Allowed
        } else {
            ActionState::Denied { reason: otherwise }
        }
    };
    let settled = |ok: bool| {
        allowed_if(
            ok,
            if rejected {
                DenialReason::AlreadyResolved
            } else {
                DenialReason::NotApplicableInState
            },
        )
    };
    [
        (MemoryAction::SetFavorite, settled(!rejected && !favorite)),
        (MemoryAction::ClearFavorite, settled(!rejected && favorite)),
        (
            MemoryAction::Validate,
            settled(status == MemoryStatus::Candidate),
        ),
        (
            MemoryAction::Promote,
            settled(status == MemoryStatus::Validated),
        ),
        (
            MemoryAction::Reject,
            if favorite && !rejected {
                ActionState::Denied {
                    reason: DenialReason::FavoriteMustBeCleared,
                }
            } else {
                settled(!rejected)
            },
        ),
    ]
    .into_iter()
    .map(|(action, state)| ActionAvailability { action, state })
    .collect()
}

/// The revision of a lesson: status rows plus favorite changes.
fn revision(database: &Database, id: &str) -> Result<Revision, WorkbenchError> {
    let rows = database.memory_history(id).map_err(storage_error)?.len();
    let favorites = database.favorite_history(id).map_err(storage_error)?.len();
    Ok(Revision((rows + favorites) as u64))
}

fn state(database: &Database, entry: &MemoryEntry) -> Result<MemoryState, WorkbenchError> {
    let status = wire_status(entry.status);
    Ok(MemoryState {
        id: MemoryId::new(&entry.lesson.id)
            .map_err(|_| error(ErrorCode::Internal, "a lesson id is not valid on the wire"))?,
        status,
        favorite: entry.favorite,
        revision: revision(database, &entry.lesson.id)?,
        actions: actions(status, entry.favorite),
    })
}

fn summary(database: &Database, entry: &MemoryEntry) -> Result<MemorySummary, WorkbenchError> {
    Ok(MemorySummary {
        state: state(database, entry)?,
        scope: wire_scope(entry.lesson.scope),
        preview: first_line(&entry.lesson.content, MAX_PREVIEW_BYTES),
    })
}

fn detail(database: &Database, entry: &MemoryEntry) -> Result<MemoryDetail, WorkbenchError> {
    Ok(MemoryDetail {
        state: state(database, entry)?,
        scope: wire_scope(entry.lesson.scope),
        task_id: first_line(&entry.lesson.task_id, 512),
        content: bounded(&entry.lesson.content),
        evidence: bounded(&entry.lesson.evidence),
    })
}
