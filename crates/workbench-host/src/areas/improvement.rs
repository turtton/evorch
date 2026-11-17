//! Host service of the `improvement` area: the self-improvement candidates of
//! a project, read (PR 13) and updated (PR 14) with the identity, revision and
//! action rules below.
//!
//! An update is one call of the storage's conditional update
//! ([`storage::StorageHandle::update_improvement`]) quoting the revision the
//! client read; see [`super::memory::update`] for the task around it. Triage
//! names no path: the core renders the candidate into a file it names itself
//! (from the candidate id, in its own draft directory) and the only write it
//! makes to the candidate is attaching that file name. Nothing is sent
//! anywhere.
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
use super::memory::{
    refusal_failure,
    update::{Done, Failure, Settled, Submitted, UpdateHook, UpdateView, Updates},
};
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
    improvement::{
        ImprovementCandidate, ImprovementSeverity as StoredSeverity,
        ImprovementSource as StoredSource, ImprovementStatus as StoredStatus,
    },
    management::{
        CasOutcome, ImprovementOp, ImprovementUpdate, improvement_denial, improvement_revision,
    },
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
            GetImprovement, ImprovementCommand, ImprovementDetail, ImprovementEvent,
            ImprovementList, ImprovementSeverity, ImprovementSource, ImprovementSummary,
            ImprovementUpdateView, ListImprovement, MAX_RECENT_RUNS,
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
    updates: Option<Arc<Updates<ImprovementUpdateView>>>,
    storage: Option<storage::StorageHandle>,
    draft_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct Wired {
    slug: SlugOf,
    database: PathBuf,
    enabled: bool,
    tasks: AreaTasks,
    source: ProjectionSource,
}

impl UpdateView for ImprovementUpdateView {
    fn settle(&mut self, outcome: &Settled) {
        super::memory::update::settle_operation(&mut self.operation, outcome);
        match outcome {
            Settled::Done(done) => {
                self.revision = Some(Revision(done.revision));
                self.changed = Some(done.changed);
                self.draft_file.clone_from(&done.draft_file);
            }
            Settled::Failed(failure) => {
                self.denial = failure.denial;
                self.message = Some(failure.message.clone());
            }
            Settled::Cancelled(_) => {
                self.message = Some(super::memory::update::cancelled_message());
            }
        }
    }
}

impl crate::areas::settings::operations::Tracked for ImprovementUpdateView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
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
        )
        .with_writer(
            handles.storage.clone(),
            crate::bootstrap::learning::draft_dir(&handles.config, &handles.database),
            None,
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
            reads: ReadOps::new(tasks.clone(), {
                let source = source.clone();
                move |view| {
                    let _ = source.publish(UiEvent::Improvement(
                        ImprovementEvent::ImprovementOperationChanged { view },
                    ));
                }
            }),
            pager: Pager::default(),
            wired: Some(Wired {
                slug,
                database,
                enabled,
                tasks,
                source,
            }),
            updates: None,
            storage: None,
            draft_dir: None,
        }
    }

    /// Let the service update candidates through the storage writer `storage`
    /// (the one conditional update). `draft_dir` is where triage saves drafts
    /// (`None` while the feature is off: triage is then refused). `hook` is a
    /// test seam awaited at each stage of every update.
    #[must_use]
    pub fn with_writer(
        mut self,
        storage: storage::StorageHandle,
        draft_dir: Option<PathBuf>,
        hook: Option<UpdateHook>,
    ) -> Self {
        if let Some(wired) = &self.wired {
            let source = wired.source.clone();
            self.updates = Some(Updates::new(
                wired.tasks.clone(),
                move |update| {
                    let _ = source.publish(UiEvent::Improvement(
                        ImprovementEvent::ImprovementUpdateCompleted { update },
                    ));
                },
                hook,
            ));
            self.storage = Some(storage);
            self.draft_dir = draft_dir;
        }
        self
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

    /// Accept an update of a candidate of the project in `root` (see the
    /// `memory` area's `submit` for the rules; the same ones apply).
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `LimitExceeded` while
    /// every retained update is running, `Draining` once the host stops,
    /// `UnsupportedCapability` for the unwired form.
    pub(crate) fn submit(
        &self,
        command: &ImprovementCommand,
        root: PathBuf,
        accepted: HostCommandPermit,
    ) -> Result<Submitted<ImprovementUpdateView>, WorkbenchError> {
        let wired = self.wired()?.clone();
        let (Some(updates), Some(storage)) = (&self.updates, &self.storage) else {
            return Err(error(
                ErrorCode::UnsupportedCapability,
                "improvement cannot be changed in this host",
            ));
        };
        let action = command.action();
        let candidate = command.candidate().clone();
        let id = candidate.as_str().to_owned();
        let expected = command.expected_revision().0;
        let storage = storage.clone();
        let draft_dir = self.draft_dir.clone();
        let request = serde_json::to_vec(command).unwrap_or_default();
        let project = command.project().clone();
        let pending_candidate = candidate.clone();
        updates.submit(
            command.operation_id(),
            &request,
            move |operation| ImprovementUpdateView {
                operation,
                project,
                candidate: pending_candidate,
                action,
                revision: None,
                changed: None,
                draft_file: None,
                denial: None,
                message: None,
            },
            Box::new(move || {
                let slug = (wired.slug)(&root);
                let database =
                    Database::open_read_only(&wired.database).map_err(|_| Failure::storage())?;
                // Membership is the one thing read (the project of a candidate
                // never changes); its state is the conditional update's to judge.
                let stored = match database.improvement_candidate(&id) {
                    Ok(Some(stored)) if stored.project == slug => stored,
                    Ok(_) => return Err(Failure::not_found("unknown candidate")),
                    Err(_) => return Err(Failure::storage()),
                };
                drop(database);
                let update = match action {
                    ImprovementAction::MarkReviewed => {
                        Prepared::Plain(ImprovementUpdate::MarkReviewed)
                    }
                    ImprovementAction::Dismiss => Prepared::Plain(ImprovementUpdate::Dismiss),
                    ImprovementAction::Triage => {
                        let dir = draft_dir.ok_or_else(|| {
                            Failure::new(
                                ErrorCode::UnsupportedCapability,
                                "self-improvement drafts are switched off in the settings",
                            )
                        })?;
                        Prepared::Draft {
                            name: draft_file_name(&id),
                            text: runtime::self_improvement::render_issue_draft(&stored),
                            dir,
                        }
                    }
                };
                Ok(Box::new(move || commit(&storage, &id, expected, update)))
            }),
            accepted,
        )
    }

    /// An update the core accepted, whatever its state.
    ///
    /// # Errors
    /// `NotFound` for an update the core does not hold (never accepted, or the
    /// core restarted since): the outcome is unknown, read the candidate instead.
    pub fn update_of(&self, id: &OperationId) -> Result<ImprovementUpdateView, WorkbenchError> {
        self.updates
            .as_ref()
            .and_then(|updates| updates.get(id))
            .ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    "unknown update; if it was sent before a restart, read the candidate to see whether it applied",
                )
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

/// What the write step of an update does.
enum Prepared {
    Plain(ImprovementUpdate),
    /// Save `text` as `dir/name`, then attach `name`.
    Draft {
        dir: PathBuf,
        name: String,
        text: String,
    },
}

/// The file a candidate's draft is saved as: the candidate id with every
/// character outside `[A-Za-z0-9._-]` replaced, so it cannot leave the draft
/// directory. The same name the passive collector uses for the same candidate.
pub fn draft_file_name(candidate_id: &str) -> String {
    let id: String = candidate_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{id}.issue.md")
}

/// The write step of an update.
///
/// A triage never touches the file the candidate points at before the writer
/// has judged it: the draft is staged in a temporary file only this operation
/// owns (unique name, created exclusively), and the writer's conditional update
/// decides what happens to it.
///
/// * `Applied`: the staged file is renamed onto the draft's name (the name was
///   not attached before, so nothing the candidate pointed at is replaced).
/// * `Unchanged`: the draft is already attached. The file stays as it is; the
///   staged one only fills the name if the file is missing (created, never
///   replaced), and is removed.
/// * refused or failed: the staged file is removed, nothing else is touched.
///
/// Two triages of one candidate share the draft's name but never a file they
/// delete: each deletes only its own staged file, and only an `Applied` one
/// (of which there is exactly one per attached name) writes the name.
fn commit(
    storage: &storage::StorageHandle,
    id: &str,
    expected: u64,
    prepared: Prepared,
) -> Result<Done, Failure> {
    let (update, staged) = match prepared {
        Prepared::Plain(update) => (update, None),
        Prepared::Draft { dir, name, text } => {
            let staged = stage_draft(&dir, &name, text.as_bytes()).map_err(|_| {
                Failure::new(
                    ErrorCode::StorageUnavailable,
                    "the draft could not be saved",
                )
            })?;
            (
                ImprovementUpdate::AttachDraft { draft_path: name },
                Some(staged),
            )
        }
    };
    let draft_file = match &update {
        ImprovementUpdate::AttachDraft { draft_path } => Some(draft_path.clone()),
        _ => None,
    };
    let outcome = storage.update_improvement(id, expected, update);
    let result = match outcome {
        Ok(CasOutcome::Applied { revision }) => {
            let published = match &staged {
                Some(staged) => staged.publish(),
                None => Ok(()),
            };
            match published {
                Ok(()) => Ok(Done {
                    revision,
                    changed: true,
                    draft_file,
                }),
                Err(_) => Err(Failure::new(
                    ErrorCode::StorageUnavailable,
                    "the draft was attached but its file could not be placed; read the candidate",
                )),
            }
        }
        Ok(CasOutcome::Unchanged { revision }) => {
            if let Some(staged) = &staged {
                staged.fill_if_missing();
            }
            Ok(Done {
                revision,
                changed: false,
                draft_file,
            })
        }
        Ok(CasOutcome::Refused(refusal)) => Err(refusal_failure(refusal, "candidate")),
        Err(_) => Err(Failure::storage()),
    };
    // Whatever happened, the staged file is this operation's own: it is gone
    // now (after a rename it no longer exists).
    if let Some(staged) = staged {
        staged.discard();
    }
    result
}

/// A draft written to a file only its operation owns, not yet at its name.
struct StagedDraft {
    temp: PathBuf,
    target: PathBuf,
}

impl StagedDraft {
    /// Put the file at its name, replacing nothing the candidate pointed at.
    fn publish(&self) -> std::io::Result<()> {
        std::fs::rename(&self.temp, &self.target)
    }

    /// Create the name when the file is missing; never replace one.
    fn fill_if_missing(&self) {
        // `AlreadyExists` is the expected answer: the draft is in place.
        let _ = std::fs::hard_link(&self.temp, &self.target);
    }

    /// Remove the staged file if it is still there.
    fn discard(self) {
        let _ = std::fs::remove_file(&self.temp);
    }
}

/// Write `bytes` to an exclusive, owner-only temporary file next to `name`.
fn stage_draft(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<StagedDraft> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let target = dir.join(name);
    let temp = dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(StagedDraft { temp, target })
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

/// What may be done to a candidate now. The update commands refuse exactly what
/// is denied here.
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
