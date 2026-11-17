//! Host service of the `memory` area: the lessons of a project, read (PR 13)
//! here and updated (PR 14, [`update`]) with the identity, revision and action
//! rules below.
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
pub mod update;

use super::{
    AreaHandles, AreaTasks, ProjectionSource,
    diagnostics::read_ops::{
        ReadOps, SlugOf, Snapshot, bounded, digest_of, first_line, page_of, runtime_slug,
    },
};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use crate::queries::{Pager, error, storage_error};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage::{
    Database,
    management::{CasOutcome, CasRefusal, Denial, MemoryOp, memory_denial},
    memory::{MemoryEntry, MemoryFilter, MemoryStatus as StoredStatus},
};
use update::{
    Done, Failure, Settled, Submitted, UpdateHook, UpdateView, Updates, cancelled_message,
    settle_operation,
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
            GetMemory, ListMemory, MAX_PREVIEW_BYTES, MemoryCommand, MemoryDetail, MemoryEvent,
            MemoryScope, MemorySummary, MemoryUpdateView,
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
    updates: Option<Arc<Updates<MemoryUpdateView>>>,
    storage: Option<storage::StorageHandle>,
}

#[derive(Clone)]
struct Wired {
    slug: SlugOf,
    database: PathBuf,
    tasks: AreaTasks,
    source: ProjectionSource,
}

impl UpdateView for MemoryUpdateView {
    fn settle(&mut self, outcome: &Settled) {
        settle_operation(&mut self.operation, outcome);
        match outcome {
            Settled::Done(done) => {
                self.revision = Some(Revision(done.revision));
                self.changed = Some(done.changed);
            }
            Settled::Failed(failure) => {
                self.denial = failure.denial;
                self.message = Some(failure.message.clone());
            }
            Settled::Cancelled(_) => self.message = Some(cancelled_message()),
        }
    }
}

impl crate::areas::settings::operations::Tracked for MemoryUpdateView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
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
        )
        .with_writer(handles.storage.clone(), None))
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
            reads: ReadOps::new(tasks.clone(), {
                let source = source.clone();
                move |view| {
                    let _ = source.publish(UiEvent::Memory(MemoryEvent::MemoryOperationChanged {
                        view,
                    }));
                }
            }),
            pager: Pager::default(),
            wired: Some(Wired {
                slug,
                database,
                tasks,
                source,
            }),
            updates: None,
            storage: None,
        }
    }

    /// Let the service update lessons through the storage writer `storage`
    /// (the one conditional update; the service never writes any other way).
    /// `hook` is a test seam awaited at each stage of every update.
    #[must_use]
    pub fn with_writer(
        mut self,
        storage: storage::StorageHandle,
        hook: Option<UpdateHook>,
    ) -> Self {
        if let Some(wired) = &self.wired {
            let source = wired.source.clone();
            self.updates = Some(Updates::new(
                wired.tasks.clone(),
                move |update| {
                    let _ = source.publish(UiEvent::Memory(MemoryEvent::MemoryUpdateCompleted {
                        update,
                    }));
                },
                hook,
            ));
            self.storage = Some(storage);
        }
        self
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

    /// Accept an update of a lesson of the project in `root`: start it, or
    /// answer from the table when the same request was accepted before.
    ///
    /// The task holds `accepted` until the outcome is settled. It reads only
    /// which project the lesson belongs to (a lesson of another project is
    /// `not_found`, like one that does not exist) and then makes the one
    /// conditional update, quoting the revision the client read.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `LimitExceeded` while
    /// every retained update is running, `Draining` once the host stops,
    /// `UnsupportedCapability` for the unwired form.
    pub(crate) fn submit(
        &self,
        command: &MemoryCommand,
        root: PathBuf,
        accepted: HostCommandPermit,
    ) -> Result<Submitted<MemoryUpdateView>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let (Some(updates), Some(storage)) = (&self.updates, &self.storage) else {
            return Err(error(
                ErrorCode::UnsupportedCapability,
                "memory cannot be changed in this host",
            ));
        };
        let update = command.update();
        let action = command.action();
        let id = update.target.id.as_str().to_owned();
        let expected = update.target.expected_revision.0;
        let op = memory_op(action);
        let storage = storage.clone();
        let request = serde_json::to_vec(command).unwrap_or_default();
        let pending_project = update.project.clone();
        let pending_id = update.target.id.clone();
        updates.submit(
            &update.operation_id,
            &request,
            move |operation| MemoryUpdateView {
                operation,
                project: pending_project,
                id: pending_id,
                action,
                revision: None,
                changed: None,
                denial: None,
                message: None,
            },
            Box::new(move || {
                let slug = (wired.slug)(&root);
                let database =
                    Database::open_read_only(&wired.database).map_err(|_| Failure::storage())?;
                // Membership is the one thing read: the project of a lesson
                // never changes. Its state is the conditional update's to judge.
                match database.memory_entry(&id) {
                    Ok(Some(entry)) if entry.lesson.project == slug => {}
                    Ok(_) => return Err(Failure::not_found("unknown lesson")),
                    Err(_) => return Err(Failure::storage()),
                }
                drop(database);
                Ok(Box::new(move || {
                    match storage.update_memory(&id, expected, op) {
                        Ok(CasOutcome::Applied { revision }) => Ok(Done {
                            revision,
                            changed: true,
                            draft_file: None,
                        }),
                        Ok(CasOutcome::Unchanged { revision }) => Ok(Done {
                            revision,
                            changed: false,
                            draft_file: None,
                        }),
                        Ok(CasOutcome::Refused(refusal)) => Err(refusal_failure(refusal, "lesson")),
                        Err(_) => Err(Failure::storage()),
                    }
                }))
            }),
            accepted,
        )
    }

    /// An update the core accepted, whatever its state.
    ///
    /// # Errors
    /// `NotFound` for an update the core does not hold (never accepted, or the
    /// core restarted since): the outcome is unknown, read the lesson instead.
    pub fn update_of(
        &self,
        id: &workbench_protocol::ids::OperationId,
    ) -> Result<MemoryUpdateView, WorkbenchError> {
        self.updates
            .as_ref()
            .and_then(|updates| updates.get(id))
            .ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    "unknown update; if it was sent before a restart, read the lesson to see whether it applied",
                )
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

fn memory_op(action: MemoryAction) -> MemoryOp {
    match action {
        MemoryAction::SetFavorite => MemoryOp::SetFavorite,
        MemoryAction::ClearFavorite => MemoryOp::ClearFavorite,
        MemoryAction::Validate => MemoryOp::Validate,
        MemoryAction::Promote => MemoryOp::Promote,
        MemoryAction::Reject => MemoryOp::Reject,
    }
}

/// How a refused conditional update is told to the client (shared with the
/// improvement area).
pub(crate) fn refusal_failure(refusal: CasRefusal, what: &str) -> Failure {
    match refusal {
        CasRefusal::NotFound => Failure::not_found(&format!("unknown {what}")),
        CasRefusal::StaleRevision { .. } => Failure::stale(what),
        CasRefusal::Denied(denial) => Failure::denied(denial_reason(denial)),
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

/// What may be done to a lesson now. The update commands refuse exactly what is
/// denied here.
///
/// The rule itself lives in [`storage::management::memory_denial`], the one the
/// writer's conditional update judges with, so what is listed here is exactly
/// what that update accepts.
pub fn actions(status: MemoryStatus, favorite: bool) -> Vec<ActionAvailability<MemoryAction>> {
    let status = stored_status(status);
    [
        (MemoryAction::SetFavorite, MemoryOp::SetFavorite),
        (MemoryAction::ClearFavorite, MemoryOp::ClearFavorite),
        (MemoryAction::Validate, MemoryOp::Validate),
        (MemoryAction::Promote, MemoryOp::Promote),
        (MemoryAction::Reject, MemoryOp::Reject),
    ]
    .into_iter()
    .map(|(action, op)| ActionAvailability {
        action,
        state: match memory_denial(op, status, favorite) {
            None => ActionState::Allowed,
            Some(denial) => ActionState::Denied {
                reason: denial_reason(denial),
            },
        },
    })
    .collect()
}

/// The wire form of a storage denial (shared with the improvement area).
pub(crate) fn denial_reason(denial: Denial) -> DenialReason {
    match denial {
        Denial::NotApplicableInState => DenialReason::NotApplicableInState,
        Denial::FavoriteMustBeCleared => DenialReason::FavoriteMustBeCleared,
        Denial::AlreadyResolved => DenialReason::AlreadyResolved,
    }
}

/// The revision of a lesson: status rows plus favorite changes, counted by the
/// storage (the value the conditional update compares).
fn revision(database: &Database, id: &str) -> Result<Revision, WorkbenchError> {
    database
        .memory_revision(id)
        .map(Revision)
        .map_err(storage_error)
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
