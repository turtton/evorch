use super::{revision, text};
use event_bus::{AgentRunPhase, UserQuestion};
use runtime::AgentRuntime;
use sandbox::approval::{ApprovalSnapshot, ApprovalState as RegistryState};
use std::{collections::BTreeMap, path::PathBuf};
use workbench_protocol::{
    envelope::{Actor, Provenance, Surface},
    handshake::{ActivityCounts, CoreState, CoreStatus},
    ids::*,
    views::*,
    wire::*,
};

#[derive(Clone, Default)]
pub(crate) struct Models {
    pub projects: BTreeMap<String, ProjectSummary>,
    pub threads: BTreeMap<String, ThreadView>,
    pub runs: BTreeMap<String, RunSummary>,
    pub questions: BTreeMap<String, QuestionView>,
    pub approvals: BTreeMap<String, ApprovalView>,
    pub receipts: BTreeMap<String, InputReceipt>,
}
impl Models {
    pub fn status(&self) -> CoreStatus {
        CoreStatus {
            state: CoreState::Ready,
            retention_reasons: vec![],
            running: ActivityCounts {
                turns: self
                    .runs
                    .values()
                    .filter(|r| r.phase == RunPhase::Running)
                    .count() as u32,
                ..Default::default()
            },
        }
    }
}
pub(crate) fn phase(phase: AgentRunPhase) -> RunPhase {
    match phase {
        AgentRunPhase::Pending => RunPhase::Pending,
        AgentRunPhase::Running => RunPhase::Running,
        AgentRunPhase::Waiting => RunPhase::WaitingForUser,
        AgentRunPhase::Stopped => RunPhase::Cancelled,
        AgentRunPhase::Done => RunPhase::Done,
        AgentRunPhase::Error => RunPhase::Failed,
    }
}
fn saved_phase(value: &str) -> RunPhase {
    match value.to_ascii_lowercase().as_str() {
        "pending" => RunPhase::Pending,
        "running" => RunPhase::Running,
        "waiting" => RunPhase::WaitingForUser,
        "done" => RunPhase::Done,
        "stopped" => RunPhase::Cancelled,
        _ => RunPhase::Failed,
    }
}
pub(crate) fn collect(
    runtime: &AgentRuntime,
    projects: &[(ProjectId, PathBuf)],
) -> Result<Models, storage::StorageError> {
    let mut m = Models::default();
    let live = runtime.list_agents();
    runtime.with_storage_reader(|db| {
        for (project, root) in projects {
            let mut cursor = None;
            loop {
                let page = db.threads_in_project(&root.to_string_lossy(), cursor.as_ref(), 200)?;
                for row in page.threads {
                    let Some(project_id) = row
                        .project_id
                        .as_deref()
                        .filter(|id| *id == project.as_str())
                        .or(Some(project.as_str()))
                    else {
                        continue;
                    };
                    let Ok(id) = ThreadId::new(&row.thread_id) else {
                        continue;
                    };
                    let lead_thread = db
                        .lane_by_thread(&row.thread_id)?
                        .and_then(|l| l.lead_thread_id)
                        .and_then(|id| ThreadId::new(id).ok());
                    let mut summary = ThreadSummary {
                        id,
                        project: ProjectId::new(project_id).unwrap(),
                        title: text(row.title.as_deref().unwrap_or("Untitled"), 1024),
                        state: ThreadState::Idle,
                        lead_thread,
                        revision: Revision(0),
                        updated_at: UnixMillis((row.updated_at_ns.max(0) / 1_000_000) as u64),
                    };
                    summary.revision = revision(&summary);
                    let root_run = row.root_run_id.as_ref().and_then(|id| RunId::new(id).ok());
                    m.threads.insert(
                        row.thread_id.clone(),
                        ThreadView {
                            summary,
                            root_run,
                            active_run: None,
                            pending_questions: 0,
                            pending_approvals: 0,
                            queued_follow_ups: 0,
                        },
                    );
                    for run in db.runs_of_thread(&row.thread_id)? {
                        if !db
                            .tombstoned_runs(std::slice::from_ref(&run.run_id))?
                            .is_empty()
                        {
                            continue;
                        }
                        if let Some(meta) = db.run_context_meta(&run.run_id)?
                            && let Ok(id) = RunId::new(&run.run_id)
                        {
                            let mut summary = RunSummary {
                                id,
                                thread: ThreadId::new(&row.thread_id).unwrap(),
                                parent_run: meta.parent_run_id.and_then(|id| RunId::new(id).ok()),
                                role: role(&meta.role),
                                phase: saved_phase(&meta.terminal_phase),
                                revision: Revision(0),
                            };
                            summary.revision = revision(&summary);
                            m.runs.insert(run.run_id.clone(), summary);
                        }
                        for question in db.user_questions_for_run(&run.run_id)? {
                            if let Some(view) = question_view(db, &question, &row.thread_id)? {
                                m.questions.insert(question.id, view);
                            }
                        }
                        let receipts = db.host_input_receipt_headers(&run.run_id)?;
                        let ids: Vec<_> = receipts.iter().map(|r| r.prompt_id.clone()).collect();
                        let submissions = db.host_chat_submissions(&ids)?;
                        for receipt in receipts {
                            let Some(sent_by) = submissions.iter().find_map(|s| match s {
                                storage::HostChatSubmissionState::Live(s)
                                    if s.submission_id == receipt.prompt_id =>
                                {
                                    serde_json::from_str::<Provenance>(&s.provenance_json)
                                        .ok()
                                        .filter(|p| p.validate().is_ok())
                                }
                                _ => None,
                            }) else {
                                continue;
                            };
                            if let (Ok(submission_id), Ok(run)) = (
                                SubmissionId::new(&receipt.prompt_id),
                                RunId::new(&receipt.run_id),
                            ) {
                                let mut view = InputReceipt {
                                    submission_id,
                                    thread: ThreadId::new(&row.thread_id).unwrap(),
                                    run: Some(run),
                                    state: match receipt.status {
                                        storage::HostInputStatus::Accepted => {
                                            InputReceiptState::Accepted
                                        }
                                        storage::HostInputStatus::Withdrawn => {
                                            InputReceiptState::Withdrawn
                                        }
                                        storage::HostInputStatus::Delivered => {
                                            InputReceiptState::Delivered
                                        }
                                        storage::HostInputStatus::Dropped => {
                                            InputReceiptState::Dropped
                                        }
                                        storage::HostInputStatus::Unknown => {
                                            InputReceiptState::Unknown
                                        }
                                    },
                                    revision: Revision(0),
                                    sent_by,
                                };
                                view.revision = revision(&view);
                                m.receipts.insert(receipt.prompt_id, view);
                            }
                        }
                    }
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
        }
        // Include live runs even before their first context snapshot.
        for agent in &live {
            let id = agent.run_id.to_string();
            if !db.tombstoned_runs(std::slice::from_ref(&id))?.is_empty() {
                continue;
            }
            let thread = db.thread_of_run(&id)?.map(|r| r.thread_id).or_else(|| {
                storage::threads::parse_chat_run_name(&agent.name).map(|(_, t)| t.to_owned())
            });
            if let Some(thread) = thread.filter(|t| m.threads.contains_key(t)) {
                let mut summary = RunSummary {
                    id: RunId::new(&id).unwrap(),
                    thread: ThreadId::new(thread).unwrap(),
                    parent_run: agent
                        .parent_run_id
                        .and_then(|r| RunId::new(r.to_string()).ok()),
                    role: role(&agent.role_name),
                    phase: phase(agent.phase),
                    revision: Revision(0),
                };
                summary.revision = revision(&summary);
                m.runs.insert(id, summary);
            }
        }
        Ok(())
    })?;
    for (id, _) in projects {
        let mut project = ProjectSummary {
            id: id.clone(),
            display_name: text(id.as_str(), 256),
            thread_count: m
                .threads
                .values()
                .filter(|t| &t.summary.project == id)
                .count() as u32,
            revision: Revision(0),
        };
        project.revision = revision(&project);
        m.projects.insert(id.as_str().to_owned(), project);
    }
    for thread in m.threads.values_mut() {
        let runs: Vec<_> = m
            .runs
            .values()
            .filter(|r| r.thread == thread.summary.id)
            .collect();
        thread.active_run = runs
            .iter()
            .find(|r| {
                matches!(
                    r.phase,
                    RunPhase::Running | RunPhase::WaitingForUser | RunPhase::WaitingForApproval
                )
            })
            .map(|r| r.id.clone());
        if thread.active_run.is_some() {
            thread.summary.state = ThreadState::Running;
        }
        thread.pending_questions = m
            .questions
            .values()
            .filter(|q| {
                q.requester.thread == thread.summary.id
                    && q.answer.is_none()
                    && q.target == QuestionTarget::User
            })
            .count() as u32;
        if thread.pending_questions > 0 {
            thread.summary.state = ThreadState::WaitingForUser;
        }
        thread.queued_follow_ups = thread
            .root_run
            .as_ref()
            .and_then(|id| id.as_str().parse::<runtime::RunId>().ok())
            .and_then(|run| runtime.follow_up_status(run).ok())
            .map_or(0, |s| s.pending as u32);
        thread.summary.revision = Revision(0);
        thread.summary.revision = revision(&thread.summary);
    }
    Ok(m)
}
fn role(value: &str) -> RunRole {
    match value.to_ascii_lowercase().as_str() {
        "worker" => RunRole::Worker,
        "orchestrator" => RunRole::Orchestrator,
        "planner" => RunRole::Planner,
        "reviewer" => RunRole::Reviewer,
        "advisor" | "oracle" => RunRole::Advisor,
        _ => RunRole::Other,
    }
}
fn question_view(
    db: &storage::Database,
    q: &UserQuestion,
    thread: &str,
) -> Result<Option<QuestionView>, storage::StorageError> {
    if !db
        .tombstoned_runs(&[q.run_id.clone(), q.root_run_id.clone()])?
        .is_empty()
    {
        return Ok(None);
    }
    let target = match &q.responder {
        Some(event_bus::lane::QuestionResponder::Lead { thread_id }) => {
            let Some(lead_run) = db
                .thread(thread_id)?
                .and_then(|t| t.root_run_id)
                .and_then(|id| RunId::new(id).ok())
            else {
                return Ok(None);
            };
            QuestionTarget::Lead {
                lead_thread: match ThreadId::new(thread_id) {
                    Ok(id) => id,
                    Err(_) => return Ok(None),
                },
                lead_run,
            }
        }
        _ if q.is_user_visible() => QuestionTarget::User,
        _ => return Ok(None),
    };
    let requester_thread = db
        .thread_of_run(&q.run_id)?
        .map(|r| r.thread_id)
        .unwrap_or_else(|| thread.to_owned());
    let (Ok(id), Ok(run), Ok(root_run), Ok(thread)) = (
        QuestionId::new(&q.id),
        RunId::new(&q.run_id),
        RunId::new(&q.root_run_id),
        ThreadId::new(requester_thread),
    ) else {
        return Ok(None);
    };
    let raised_by = Provenance::agent(run.clone());
    let answer = q.answer.as_ref().map(|answer| QuestionAnswer {
        text: text(answer, 256 * 1024),
        answered_by: match &q.answer_origin {
            Some(event_bus::AnswerOrigin::ParentAgent { answered_by_run_id }) => {
                RunId::new(answered_by_run_id).ok().map(Provenance::agent)
            }
            Some(event_bus::AnswerOrigin::LaneLead { run_id, .. }) => {
                RunId::new(run_id).ok().map(Provenance::agent)
            }
            Some(event_bus::AnswerOrigin::Host { .. }) => None,
            _ => None, // An unknown source is never fabricated as human approval.
        },
    });
    Ok(Some(QuestionView {
        id,
        requester: QuestionRequester {
            run,
            thread,
            root_run,
        },
        target,
        management_revision: Revision(q.management_revision.unwrap_or(0)),
        routing_revision: Revision(q.routing_revision),
        blocking: q.blocking,
        title: text(&q.title, 1024),
        options: q.options.iter().take(32).map(|o| text(o, 1024)).collect(),
        answer,
        revision: Revision(q.routing_revision),
        raised_by,
    }))
}
pub(crate) fn approval_view(a: &ApprovalSnapshot) -> Option<ApprovalView> {
    use sandbox::approval::{ApprovalActor, ApprovalSurface};
    let summary = text(&a.summary, 1024);
    let summary_truncated =
        a.summary.len() < a.input_bytes || summary.len() < super::masked(&a.summary).len();
    Some(ApprovalView {
        id: ApprovalId::new(&a.id.value).ok()?,
        core_epoch: InstanceEpoch(a.id.core_epoch),
        revision: Revision(a.revision),
        thread: ThreadId::new(a.thread_id.as_ref()?).ok()?,
        run: RunId::new(&a.run_id).ok()?,
        tool_name: text(&a.tool_name, 256),
        summary,
        summary_truncated,
        input_bytes: ByteOffset(a.input_bytes as u64),
        state: match a.state {
            RegistryState::Pending => ApprovalState::Pending,
            RegistryState::Approved => ApprovalState::Approved,
            RegistryState::Denied => ApprovalState::Denied,
            RegistryState::Expired | RegistryState::Cancelled => ApprovalState::Expired,
        },
        decided_by: a
            .decided_by
            .filter(|p| p.actor == ApprovalActor::Human && p.surface != ApprovalSurface::None)
            .map(|p| Provenance {
                actor: match p.actor {
                    ApprovalActor::Human => Actor::Human,
                    ApprovalActor::Agent => Actor::Agent,
                },
                surface: match p.surface {
                    ApprovalSurface::Desktop => Surface::Desktop,
                    ApprovalSurface::Web => Surface::Web,
                    ApprovalSurface::Cli => Surface::Cli,
                    ApprovalSurface::None => Surface::None,
                },
                client_id: None,
                agent_run_id: None,
            }),
    })
}
