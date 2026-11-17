//! Host service of the `improvement` area, read side (PR 13): the
//! self-improvement candidates of a project. PR 14 adds the update side here,
//! with the identity, revision and action rules below.
//!
//! Reads are operations of [`read_ops`](super::diagnostics::read_ops) (see the
//! `memory` area for the shape): the database is opened read-only on a blocking
//! worker, and the project's repository slug is resolved there too.
//!
//! The storage has no revision column, so [`revision_of`] derives one from the
//! fields an update or a new occurrence changes (status, occurrences, last
//! seen, reopening, draft); it is a content digest, to be compared for
//! equality only. The draft is named by its file name; its directory (which may
//! be an absolute host path) never leaves the core.
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
    improvement::{
        ImprovementCandidate, ImprovementSeverity as StoredSeverity,
        ImprovementSource as StoredSource, ImprovementStatus as StoredStatus,
    },
    management::{ImprovementOp, improvement_denial, improvement_revision},
};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    events::UiEvent,
    ids::{ImprovementCandidateId, OperationId, RunId},
    management::{
        ActionAvailability, ActionState, ImprovementAction, ImprovementState, ImprovementStatus,
    },
    namespaces::{
        diagnostics::ReadOutcome,
        improvement::{
            GetImprovement, ImprovementDetail, ImprovementEvent, ImprovementList,
            ImprovementSeverity, ImprovementSource, ImprovementSummary, ListImprovement,
            MAX_RECENT_RUNS,
        },
    },
    operation::OperationView,
    wire::Revision,
};

/// Most candidates one list reads (the storage caps a read at 1000).
const LIST_LIMIT: usize = 500;

/// What a finished read holds, before it is cut into pages.
#[derive(Clone)]
pub enum ImprovementRead {
    List {
        enabled: bool,
        candidates: Vec<ImprovementSummary>,
    },
    Detail(Box<ImprovementDetail>),
}

/// The `improvement` service, reached from `ingress/namespaces/improvement.rs`
/// as `self.areas.improvement`. Without handles every read is refused.
#[derive(Default)]
pub struct ImprovementArea {
    reads: ReadOps<ImprovementRead>,
    pager: Pager,
    wired: Option<Wired>,
}

#[derive(Clone)]
struct Wired {
    slug: SlugOf,
    database: PathBuf,
    enabled: bool,
}

impl ImprovementArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("improvement")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::from_parts(
            handles.tasks.clone(),
            source,
            handles.database.clone(),
            runtime_slug(handles.runtime.clone()),
            handles.config.self_improvement.enabled,
        ))
    }

    /// The service over its parts: the host's tasks, the namespace's event
    /// source, the event database file, the candidate project of a directory
    /// and whether the feature is switched on in the settings.
    pub fn from_parts(
        tasks: AreaTasks,
        source: ProjectionSource,
        database: PathBuf,
        slug: SlugOf,
        enabled: bool,
    ) -> Self {
        Self {
            reads: ReadOps::new(tasks, move |view| {
                let _ = source.publish(UiEvent::Improvement(
                    ImprovementEvent::ImprovementOperationChanged { view },
                ));
            }),
            pager: Pager::default(),
            wired: Some(Wired {
                slug,
                database,
                enabled,
            }),
        }
    }

    fn wired(&self) -> Result<&Wired, WorkbenchError> {
        self.wired.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "improvement is not available in this host",
            )
        })
    }

    /// Start or join a list of the candidates of the project in `root`.
    ///
    /// # Errors
    /// See [`ReadOps::submit`]; `Conflict` also for a cursor that is not this
    /// operation's.
    pub fn list(
        &self,
        root: PathBuf,
        list: &ListImprovement,
    ) -> Result<ReadOutcome<ImprovementList>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let mut identity = list.clone();
        identity.page = Default::default();
        let status = list.status.map(stored_status);
        let snapshot = self.reads.submit(
            &list.operation_id,
            digest_of(&("list", &identity)),
            move || {
                let slug = (wired.slug)(&root);
                let database = open(&wired.database)?;
                let candidates = database
                    .improvement_candidates(&slug, status, LIST_LIMIT)
                    .map_err(storage_error)?
                    .iter()
                    .map(summary)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ImprovementRead::List {
                    enabled: wired.enabled,
                    candidates,
                })
            },
        )?;
        let data = match snapshot.data {
            Some(ImprovementRead::List {
                enabled,
                candidates,
            }) => Some(ImprovementList {
                enabled,
                candidates: page_of(&self.pager, &list.operation_id, &candidates, &list.page)?,
            }),
            _ => None,
        };
        Ok(ReadOutcome {
            operation: snapshot.view,
            data,
        })
    }

    /// Start or join the read of one candidate.
    ///
    /// # Errors
    /// See [`ReadOps::submit`].
    pub fn get(
        &self,
        root: PathBuf,
        get: &GetImprovement,
    ) -> Result<ReadOutcome<ImprovementDetail>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let id = get.candidate.as_str().to_owned();
        let Snapshot { view, data } = self.reads.submit(
            &get.operation_id,
            digest_of(&("get", &get.project, &get.candidate)),
            move || {
                let slug = (wired.slug)(&root);
                let database = open(&wired.database)?;
                let candidate = database
                    .improvement_candidate(&id)
                    .map_err(storage_error)?
                    .filter(|candidate| candidate.project == slug)
                    .ok_or_else(|| error(ErrorCode::NotFound, "unknown candidate"))?;
                Ok(ImprovementRead::Detail(Box::new(detail(&candidate)?)))
            },
        )?;
        Ok(ReadOutcome {
            operation: view,
            data: match data {
                Some(ImprovementRead::Detail(detail)) => Some(*detail),
                _ => None,
            },
        })
    }

    /// The view of an operation of this area.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not know.
    pub fn operation(&self, id: &OperationId) -> Result<OperationView, WorkbenchError> {
        self.reads.view(id)
    }
}

fn open(path: &Path) -> Result<Database, WorkbenchError> {
    Database::open_read_only(path).map_err(storage_error)
}

fn stored_status(status: ImprovementStatus) -> StoredStatus {
    match status {
        ImprovementStatus::New => StoredStatus::New,
        ImprovementStatus::Reviewed => StoredStatus::Reviewed,
        ImprovementStatus::Dismissed => StoredStatus::Dismissed,
    }
}

fn wire_status(status: StoredStatus) -> ImprovementStatus {
    match status {
        StoredStatus::New => ImprovementStatus::New,
        StoredStatus::Reviewed => ImprovementStatus::Reviewed,
        StoredStatus::Dismissed => ImprovementStatus::Dismissed,
    }
}

/// What may be done to a candidate now. PR 14's commands refuse exactly what is
/// denied here.
///
/// The rule itself lives in [`storage::management::improvement_denial`], the one
/// the writer's conditional update judges with.
pub fn actions(status: ImprovementStatus) -> Vec<ActionAvailability<ImprovementAction>> {
    let status = stored_status(status);
    [
        (ImprovementAction::MarkReviewed, ImprovementOp::MarkReviewed),
        (ImprovementAction::Dismiss, ImprovementOp::Dismiss),
        (ImprovementAction::Triage, ImprovementOp::Triage),
    ]
    .into_iter()
    .map(|(action, op)| ActionAvailability {
        action,
        state: match improvement_denial(op, status) {
            None => ActionState::Allowed,
            Some(denial) => ActionState::Denied {
                reason: super::memory::denial_reason(denial),
            },
        },
    })
    .collect()
}

/// The revision of a candidate (see the module documentation), computed by the
/// storage: the value the conditional update compares.
pub fn revision_of(candidate: &ImprovementCandidate) -> Revision {
    Revision(improvement_revision(candidate))
}

fn millis(ns: u64) -> u64 {
    ns / 1_000_000
}

fn summary(candidate: &ImprovementCandidate) -> Result<ImprovementSummary, WorkbenchError> {
    let status = wire_status(candidate.status);
    Ok(ImprovementSummary {
        state: ImprovementState {
            id: ImprovementCandidateId::new(&candidate.id).map_err(|_| {
                error(
                    ErrorCode::Internal,
                    "a candidate id is not valid on the wire",
                )
            })?,
            status,
            revision: revision_of(candidate),
            actions: actions(status),
        },
        source: match candidate.source {
            StoredSource::Diagnostic => ImprovementSource::Diagnostic,
            StoredSource::Lesson => ImprovementSource::Lesson,
        },
        severity: match candidate.severity {
            StoredSeverity::Info => ImprovementSeverity::Info,
            StoredSeverity::Warning => ImprovementSeverity::Warning,
            StoredSeverity::Error => ImprovementSeverity::Error,
        },
        code: first_line(&candidate.code, 256),
        title: first_line(&candidate.title, 1024),
        occurrences: candidate.occurrences,
        last_seen_ms: millis(candidate.last_seen_at_ns),
    })
}

fn detail(candidate: &ImprovementCandidate) -> Result<ImprovementDetail, WorkbenchError> {
    let run = |id: &String| RunId::new(id).ok();
    Ok(ImprovementDetail {
        summary: summary(candidate)?,
        evidence: bounded(&candidate.evidence),
        draft_file: candidate.draft_path.as_deref().and_then(|path| {
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| first_line(name, 256))
        }),
        run: candidate.run_id.as_ref().and_then(run),
        recent_runs: candidate
            .recent_run_ids
            .iter()
            .rev()
            .take(MAX_RECENT_RUNS)
            .rev()
            .filter_map(run)
            .collect(),
        created_ms: millis(candidate.created_at_ns),
        reopened_count: candidate.reopened_count,
    })
}
