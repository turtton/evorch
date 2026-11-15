//! Host service of the `evaluation` area, read side (PR 13): which evaluation
//! groups exist, how their members stand, and the report a member left. PR 17c
//! adds the jobs (run, report, rejudge) to this file.
//!
//! The files are read through the shared
//! [`EvaluationInspector`](eval::inspection::EvaluationInspector), which confines
//! every path to the evaluation root, follows no symbolic link and bounds every
//! read. The reads are operations of
//! [`read_ops`](super::diagnostics::read_ops): the inspector runs on a blocking
//! worker, never under the host's mutex.
//!
//! * A file that is not there is `Missing`; one that is there and unusable is
//!   `Problem` with a [`EvaluationProblemKind`]. The reader's own error text
//!   names paths and is not forwarded.
//! * Nothing is generated: a report is read, never made; a member without one
//!   stays `Missing` until the evaluation writes it.
//! * The inspector is built without a launch monitor, so a member the core
//!   itself launched is reported from its files like any other (its process
//!   status is `Unverified` until its report appears).
use super::{
    AreaHandles, AreaTasks, ProjectionSource,
    diagnostics::read_ops::{ReadOps, bounded, digest_of, first_line, page_of},
};
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, error, revision};
use eval::inspection::{
    Artifact, EvaluationGroupSnapshot, EvaluationInspector, FileState, GroupPhase, MemberPhase,
    ProblemKind,
};
use std::path::PathBuf;
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    events::UiEvent,
    ids::{EvaluationGroupId, EvaluationMemberId, OperationId},
    management::{ArtifactPresence, EvaluationArtifactKind, EvaluationArtifactRef},
    namespaces::{
        diagnostics::{MAX_TEXT_BYTES, ReadOutcome},
        evaluation::{
            EvaluationEvent, EvaluationGroupDetail, EvaluationGroupPhase, EvaluationGroupSummary,
            EvaluationList, EvaluationMemberPhase, EvaluationMemberSummary, EvaluationProblemKind,
            EvaluationReportText, EvaluationTrialCounts, GetEvaluation, GetEvaluationReport,
            ListEvaluations, MAX_EVALUATION_MEMBERS,
        },
    },
    operation::OperationView,
    wire::Revision,
};

/// Groups one list asks the inspector for (it applies its own, smaller bound).
const LIST_LIMIT: usize = 200;

/// What a finished read holds, before it is cut into pages.
#[derive(Clone)]
pub enum EvaluationRead {
    List {
        groups: Vec<EvaluationGroupSummary>,
        omitted: u32,
        scan_truncated: bool,
        problem: Option<EvaluationProblemKind>,
    },
    Group(Box<EvaluationGroupDetail>),
    Report(Box<EvaluationReportText>),
}

/// The `evaluation` service, reached from `ingress/namespaces/evaluation.rs` as
/// `self.areas.evaluation`. Without handles every read is refused.
#[derive(Default)]
pub struct EvaluationArea {
    reads: ReadOps<EvaluationRead>,
    pager: Pager,
    root: Option<PathBuf>,
}

impl EvaluationArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("evaluation")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::from_parts(
            handles.tasks.clone(),
            source,
            handles.config_dir.join("eval"),
        ))
    }

    /// The service over its parts: the host's tasks, the namespace's event
    /// source and the evaluation root directory.
    pub fn from_parts(tasks: AreaTasks, source: ProjectionSource, root: PathBuf) -> Self {
        Self {
            reads: ReadOps::new(tasks, move |view| {
                let _ = source.publish(UiEvent::Evaluation(
                    EvaluationEvent::EvaluationOperationChanged { view },
                ));
            }),
            pager: Pager::default(),
            root: Some(root),
        }
    }

    fn inspector(&self) -> Result<EvaluationInspector, WorkbenchError> {
        self.root
            .as_ref()
            .map(EvaluationInspector::new)
            .ok_or_else(|| {
                error(
                    ErrorCode::UnsupportedCapability,
                    "evaluation is not available in this host",
                )
            })
    }

    /// Start or join a list of the evaluation groups, newest first.
    ///
    /// # Errors
    /// See [`ReadOps::submit`]; `Conflict` also for a cursor that is not this
    /// operation's.
    pub fn list(
        &self,
        list: &ListEvaluations,
    ) -> Result<ReadOutcome<EvaluationList>, WorkbenchError> {
        let inspector = self.inspector()?;
        let snapshot = self
            .reads
            .submit(&list.operation_id, digest_of(&"list"), move || {
                let listing = inspector.list_groups(LIST_LIMIT);
                let groups = listing
                    .entries
                    .iter()
                    .map(|entry| group_summary(&entry.id, &entry.group))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(EvaluationRead::List {
                    groups,
                    omitted: u32::try_from(listing.omitted).unwrap_or(u32::MAX),
                    scan_truncated: listing.scan_truncated,
                    problem: listing
                        .problem
                        .as_ref()
                        .map(|problem| problem_kind(problem.kind)),
                })
            })?;
        let data = match snapshot.data {
            Some(EvaluationRead::List {
                groups,
                omitted,
                scan_truncated,
                problem,
            }) => Some(EvaluationList {
                groups: page_of(&self.pager, &list.operation_id, &groups, &list.page)?,
                omitted,
                scan_truncated,
                problem,
            }),
            _ => None,
        };
        Ok(ReadOutcome {
            operation: snapshot.view,
            data,
        })
    }

    /// Start or join the read of one group with its members.
    ///
    /// # Errors
    /// See [`ReadOps::submit`].
    pub fn get(
        &self,
        get: &GetEvaluation,
    ) -> Result<ReadOutcome<EvaluationGroupDetail>, WorkbenchError> {
        let inspector = self.inspector()?;
        let group = get.group.as_str().to_owned();
        let snapshot = self.reads.submit(
            &get.operation_id,
            digest_of(&("get", &get.group)),
            move || {
                let snapshot = present(inspector.group(&group))?;
                group_detail(&snapshot).map(|detail| EvaluationRead::Group(Box::new(detail)))
            },
        )?;
        Ok(ReadOutcome {
            operation: snapshot.view,
            data: match snapshot.data {
                Some(EvaluationRead::Group(detail)) => Some(*detail),
                _ => None,
            },
        })
    }

    /// Start or join the read of the start of a member's `report.md`.
    ///
    /// # Errors
    /// See [`ReadOps::submit`].
    pub fn report(
        &self,
        get: &GetEvaluationReport,
    ) -> Result<ReadOutcome<EvaluationReportText>, WorkbenchError> {
        let inspector = self.inspector()?;
        let (group, member) = (get.group.clone(), get.member.clone());
        let snapshot = self.reads.submit(
            &get.operation_id,
            digest_of(&("report", &get.group, &get.member)),
            move || {
                let snapshot = present(inspector.group(group.as_str()))?;
                let index = snapshot
                    .members
                    .iter()
                    .position(|candidate| candidate.dir_name == member.as_str())
                    .ok_or_else(|| error(ErrorCode::NotFound, "unknown member"))?;
                let artifact = |presence| EvaluationArtifactRef {
                    group: group.clone(),
                    member: member.clone(),
                    kind: EvaluationArtifactKind::Report,
                    presence,
                };
                let report = match inspector.member_report_markdown(
                    group.as_str(),
                    index,
                    MAX_TEXT_BYTES as u64,
                ) {
                    Artifact::Present(text) => {
                        let mut out = bounded(&text.text);
                        out.total_bytes = out.total_bytes.max(text.total_bytes);
                        out.truncated |= text.truncated || out.total_bytes > out.text.len() as u64;
                        EvaluationReportText {
                            artifact: artifact(ArtifactPresence::Present),
                            text: Some(out),
                            problem: None,
                        }
                    }
                    Artifact::Missing => EvaluationReportText {
                        artifact: artifact(ArtifactPresence::Missing),
                        text: None,
                        problem: None,
                    },
                    Artifact::Problem(problem) => EvaluationReportText {
                        artifact: artifact(ArtifactPresence::Problem),
                        text: None,
                        problem: Some(problem_kind(problem.kind)),
                    },
                };
                Ok(EvaluationRead::Report(Box::new(report)))
            },
        )?;
        Ok(ReadOutcome {
            operation: snapshot.view,
            data: match snapshot.data {
                Some(EvaluationRead::Report(report)) => Some(*report),
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

/// A group record, or why the operation fails: nothing is there (`NotFound`) or
/// it cannot be read (`StorageUnavailable`; the reason stays in the core).
fn present(
    group: Artifact<EvaluationGroupSnapshot>,
) -> Result<EvaluationGroupSnapshot, WorkbenchError> {
    match group {
        Artifact::Present(group) => Ok(group),
        Artifact::Missing => Err(error(ErrorCode::NotFound, "unknown evaluation group")),
        Artifact::Problem(_) => Err(error(
            ErrorCode::StorageUnavailable,
            "the evaluation group cannot be read",
        )),
    }
}

fn problem_kind(kind: ProblemKind) -> EvaluationProblemKind {
    match kind {
        ProblemKind::Malformed => EvaluationProblemKind::Malformed,
        ProblemKind::UnsupportedSchema => EvaluationProblemKind::UnsupportedSchema,
        ProblemKind::TooLarge => EvaluationProblemKind::TooLarge,
        ProblemKind::PermissionDenied => EvaluationProblemKind::PermissionDenied,
        ProblemKind::UnsafePath => EvaluationProblemKind::UnsafePath,
        ProblemKind::Io => EvaluationProblemKind::Io,
    }
}

fn group_phase(phase: GroupPhase) -> EvaluationGroupPhase {
    match phase {
        GroupPhase::Running => EvaluationGroupPhase::Running,
        GroupPhase::Finished => EvaluationGroupPhase::Finished,
        GroupPhase::Incomplete => EvaluationGroupPhase::Incomplete,
        GroupPhase::Unverified => EvaluationGroupPhase::Unverified,
        GroupPhase::Partial => EvaluationGroupPhase::Partial,
    }
}

fn member_phase(phase: MemberPhase) -> EvaluationMemberPhase {
    match phase {
        MemberPhase::Starting => EvaluationMemberPhase::Starting,
        MemberPhase::RunningTrials => EvaluationMemberPhase::RunningTrials,
        MemberPhase::Reporting => EvaluationMemberPhase::Reporting,
        MemberPhase::Finished => EvaluationMemberPhase::Finished,
        MemberPhase::FinishedWithoutReport => EvaluationMemberPhase::FinishedWithoutReport,
        MemberPhase::Failed => EvaluationMemberPhase::Failed,
        MemberPhase::Unverified => EvaluationMemberPhase::Unverified,
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn group_id(id: &str) -> Result<EvaluationGroupId, WorkbenchError> {
    EvaluationGroupId::new(id)
        .map_err(|_| error(ErrorCode::Internal, "a group id is not valid on the wire"))
}

fn group_summary(
    id: &str,
    group: &Artifact<EvaluationGroupSnapshot>,
) -> Result<EvaluationGroupSummary, WorkbenchError> {
    let mut summary = EvaluationGroupSummary {
        group: group_id(id)?,
        presence: ArtifactPresence::Missing,
        problem: None,
        phase: None,
        started_at: None,
        finished_at: None,
        members_total: None,
        revision: Revision(0),
    };
    match group {
        Artifact::Present(group) => {
            summary.presence = ArtifactPresence::Present;
            summary.phase = Some(group_phase(group.phase));
            summary.started_at = Some(first_line(&group.started_at, 256));
            summary.finished_at = group.finished_at.as_deref().map(|at| first_line(at, 256));
            summary.members_total = Some(count(group.members_total));
        }
        Artifact::Missing => {}
        Artifact::Problem(problem) => {
            summary.presence = ArtifactPresence::Problem;
            summary.problem = Some(problem_kind(problem.kind));
        }
    }
    summary.revision = revision(&summary);
    Ok(summary)
}

fn artifact_presence(state: &FileState) -> ArtifactPresence {
    match state {
        FileState::Present { .. } => ArtifactPresence::Present,
        FileState::Missing => ArtifactPresence::Missing,
        FileState::Rejected(_) => ArtifactPresence::Problem,
    }
}

fn member_summary(
    group: &EvaluationGroupId,
    member: &eval::inspection::EvaluationMemberSnapshot,
) -> Result<EvaluationMemberSummary, WorkbenchError> {
    let id = EvaluationMemberId::new(&member.dir_name)
        .map_err(|_| error(ErrorCode::Internal, "a member id is not valid on the wire"))?;
    let report = member
        .report_markdown
        .as_ref()
        .map_or(ArtifactPresence::Missing, |file| {
            artifact_presence(&file.state)
        });
    let conversations = member
        .trials
        .iter()
        .filter_map(|trial| match trial.artifacts.as_ref()? {
            Artifact::Present(files) => Some(artifact_presence(&files.conversation.state)),
            Artifact::Missing => Some(ArtifactPresence::Missing),
            Artifact::Problem(_) => Some(ArtifactPresence::Problem),
        })
        .collect::<Vec<_>>();
    let replay = if conversations.contains(&ArtifactPresence::Present) {
        ArtifactPresence::Present
    } else if conversations.contains(&ArtifactPresence::Problem) {
        ArtifactPresence::Problem
    } else {
        ArtifactPresence::Missing
    };
    let tally = &member.tally;
    Ok(EvaluationMemberSummary {
        member: id.clone(),
        index: count(member.index),
        thread_id: first_line(&member.thread_id, 256),
        phase: member_phase(member.phase),
        trials: EvaluationTrialCounts {
            planned: count(tally.planned),
            queued: count(tally.queued),
            running: count(tally.running),
            finished: count(tally.finished),
            failed: count(tally.failed),
            interrupted: count(tally.interrupted),
            unavailable: count(tally.unavailable),
            unverified: count(tally.unverified),
        },
        report: EvaluationArtifactRef {
            group: group.clone(),
            member: id.clone(),
            kind: EvaluationArtifactKind::Report,
            presence: report,
        },
        replay: EvaluationArtifactRef {
            group: group.clone(),
            member: id,
            kind: EvaluationArtifactKind::Replay,
            presence: replay,
        },
        failure: member
            .failure
            .as_deref()
            .map(|failure| first_line(failure, 1024)),
        problems: count(member.problems.len()),
    })
}

fn group_detail(group: &EvaluationGroupSnapshot) -> Result<EvaluationGroupDetail, WorkbenchError> {
    let id = group_id(&group.id)?;
    let members = group
        .members
        .iter()
        .take(MAX_EVALUATION_MEMBERS)
        .map(|member| member_summary(&id, member))
        .collect::<Result<Vec<_>, _>>()?;
    let mut detail = EvaluationGroupDetail {
        group: id,
        phase: group_phase(group.phase),
        started_at: first_line(&group.started_at, 256),
        finished_at: group.finished_at.as_deref().map(|at| first_line(at, 256)),
        notice: group
            .notice
            .as_deref()
            .map(|notice| first_line(notice, 1024)),
        candidate: group.candidate.present().map(|text| {
            let mut out = bounded(&text.text);
            out.total_bytes = out.total_bytes.max(text.total_bytes);
            out.truncated |= text.truncated || out.total_bytes > out.text.len() as u64;
            out
        }),
        members_omitted: count(
            group.members_omitted + group.members.len().saturating_sub(MAX_EVALUATION_MEMBERS),
        ),
        members_total: count(group.members_total),
        members,
        revision: Revision(0),
    };
    detail.revision = revision(&detail);
    Ok(detail)
}
