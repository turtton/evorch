//! Execution namespace adapter: stopping runs, goals, execution settings and
//! CLI invocations.
use workbench_protocol::commands::{CommandReply, CommandResult, ComposerRole, SendChat};
use workbench_protocol::envelope::{Actor, Provenance, Surface};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::ids::{ClientId, ProjectId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::ExecutionCommand;
use workbench_protocol::namespaces::execution::invocation::{
    EndInvocation, InvocationOwnership, InvocationRole, InvocationStarted, StartInvocation,
};
use workbench_protocol::namespaces::execution::{ExecutionReply, ExecutionResult};
use workbench_protocol::queries::QueryReply;

use crate::bootstrap::restart::CommandAccess;
use crate::commands::{HostCommand, InvocationRun, RejectKind};
use crate::ingress::{CommandHost, reject_error};
use crate::invocations::{InvocationError, NewInvocation, SentInput};

impl CommandHost {
    pub(in crate::ingress) fn execution_command(
        &mut self,
        command: ExecutionCommand,
        provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let host_command = match command {
            ExecutionCommand::StopChat(target) => HostCommand::StopChat {
                thread_id: target.thread.as_str().to_owned(),
            },
            ExecutionCommand::StopRun(stop) => HostCommand::StopRun {
                thread_id: stop.thread.as_str().to_owned(),
                run_id: stop.run.as_str().to_owned(),
            },
            ExecutionCommand::SubmitGoal(goal) => HostCommand::SubmitGoal(self.goal_input(&goal)?),
            ExecutionCommand::SetGoalReview(set) => HostCommand::SetGoalReview {
                thread_id: set.thread.as_str().to_owned(),
                goal_id: set.goal.as_str().to_owned(),
                enabled: set.enabled,
            },
            ExecutionCommand::SetGoalChecksPaused(set) => HostCommand::SetGoalChecksPaused {
                thread_id: set.thread.as_str().to_owned(),
                goal_id: set.goal.as_str().to_owned(),
                paused: set.paused,
            },
            ExecutionCommand::SetAgentWebToolsEnabled(set) => HostCommand::SetWebToolsEnabled {
                enabled: set.enabled,
            },
            ExecutionCommand::StartInvocation(start) => {
                return self.start_invocation(&start, provenance);
            }
            ExecutionCommand::EndInvocation(end) => {
                return self.end_invocation(&end, provenance);
            }
        };
        self.run_host_command(host_command, provenance)
    }
}

/// The client of an invocation request: a human on the local CLI surface.
/// A project root is a path of the machine that runs the core, so no other
/// surface (the Web, an agent) may name one.
fn invocation_client(provenance: &Provenance) -> Result<ClientId, WorkbenchError> {
    match (&provenance.client_id, provenance.surface, provenance.actor) {
        (Some(client), Surface::Cli, Actor::Human) => Ok(client.clone()),
        _ => Err(WorkbenchError::new(
            ErrorCode::PermissionDenied,
            "invocations are only available to the local command line",
            RetryAdvice::Never,
        )),
    }
}

fn invocation_role(role: InvocationRole) -> runtime::Role {
    match role {
        InvocationRole::Orchestrator => runtime::Role::Orchestrator,
        InvocationRole::Explorer => runtime::Role::Explorer,
        InvocationRole::Worker => runtime::Role::Worker,
        InvocationRole::Reviewer => runtime::Role::Reviewer,
        InvocationRole::WebResearcher => runtime::Role::WebResearcher,
        InvocationRole::Planner => runtime::Role::Planner,
        InvocationRole::Advisor => runtime::Role::Advisor,
    }
}

fn started_reply(started: InvocationStarted) -> CommandReply {
    let run = started.run.clone();
    CommandReply {
        run_id: Some(run),
        ..CommandReply::with_result(CommandResult::Execution(
            ExecutionResult::InvocationStarted(started),
        ))
    }
}

fn title_of(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default();
    line.chars().take(80).collect()
}

impl CommandHost {
    fn project_for_root(&self, root: &std::path::Path) -> Option<ProjectId> {
        self.projects
            .entries()
            .into_iter()
            .find(|(_, path)| std::fs::canonicalize(path).is_ok_and(|path| path == root))
            .map(|(id, _)| id)
    }

    fn start_invocation(
        &mut self,
        start: &StartInvocation,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let client = invocation_client(provenance)?;
        let fingerprint = serde_json::to_string(start).map_err(|_| {
            reject_error(
                RejectKind::Invalid,
                "the invocation cannot be fingerprinted",
            )
        })?;
        let registry = self.service.invocations().clone();
        registry.install_hook(&self.areas.connections);
        if let Some(existing) = registry.find(&start.invocation) {
            if existing.client != client || existing.fingerprint != fingerprint {
                return Err(reject_error(
                    RejectKind::Conflict,
                    "the invocation id was used for a different request",
                ));
            }
            return Ok(started_reply(InvocationStarted {
                invocation: start.invocation.clone(),
                thread: ThreadId::new(&existing.thread)
                    .map_err(|_| reject_error(RejectKind::Failed, "invalid thread"))?,
                run: workbench_protocol::ids::RunId::new(existing.run.to_string())
                    .map_err(|_| reject_error(RejectKind::Failed, "invalid run"))?,
                ownership: existing.ownership,
                already_started: true,
            }));
        }
        let root = std::fs::canonicalize(&start.project_root)
            .ok()
            .filter(|root| root.is_dir())
            .ok_or_else(|| reject_error(RejectKind::NotFound, "project root is unavailable"))?;
        let epoch = self.areas.connections.epoch_of(&client).ok_or_else(|| {
            reject_error(
                RejectKind::Unavailable,
                "an invocation needs a live connection",
            )
        })?;
        let lease = registry.admit().map_err(|_| {
            reject_error(
                RejectKind::Busy,
                "host is preparing restart; the invocation was not started",
            )
        })?;
        let submission = format!("cli-{}", start.invocation.as_str());
        let thread = match &start.thread {
            Some(thread) => thread.clone(),
            None => ThreadId::new(format!("cli-{}", uuid::Uuid::new_v4()))
                .map_err(|_| reject_error(RejectKind::Failed, "thread id"))?,
        };
        // A lane's thread has one root, the ledger's: an invocation aimed at it is input to
        // that root whatever the role asked for, never a run of its own.
        let input_mode = start.thread.is_some()
            && (matches!(
                self.chat_access(thread.as_str()),
                CommandAccess::ExistingInput
            ) || !matches!(
                self.service
                    .runtime()
                    .lane_continuation_root(thread.as_str()),
                Ok(None)
            ));
        let (run, owned, input) = if input_mode {
            let project = self.project_for_root(&root).ok_or_else(|| {
                reject_error(
                    RejectKind::NotFound,
                    "project is not registered with this core",
                )
            })?;
            let send = SendChat {
                project,
                thread: thread.clone(),
                submission_id: SubmissionId::new(&submission)
                    .map_err(|_| reject_error(RejectKind::Failed, "submission id"))?,
                composer_role: ComposerRole::default(),
                text: start.prompt.clone(),
                skills: Vec::new(),
                attachments: Vec::new(),
                model_preference: None,
                fork_seed: None,
            };
            let reply = self.send_chat(&send, provenance)?;
            let run = reply
                .run_id
                .as_ref()
                .and_then(|run| run.as_str().parse::<runtime::RunId>().ok())
                .ok_or_else(|| {
                    reject_error(RejectKind::Failed, "the run of the input is unknown")
                })?;
            (
                run,
                None,
                Some(SentInput {
                    run,
                    submission: submission.clone(),
                }),
            )
        } else {
            if start.thread.is_none()
                && let Some(project) = self.project_for_root(&root)
            {
                self.reserve_new_thread(
                    &project,
                    &thread,
                    &title_of(&start.prompt),
                    start.role == InvocationRole::Orchestrator,
                    None,
                )?;
            }
            let run = self.service.start_invocation_run(InvocationRun {
                thread: thread.as_str().to_owned(),
                role: invocation_role(start.role),
                prompt: start.prompt.clone(),
                project_root: root,
                submission_id: submission,
            })?;
            (run, Some(run), None)
        };
        let ownership = if owned.is_some() {
            InvocationOwnership::OwnsRun
        } else {
            InvocationOwnership::InputOnly
        };
        registry.record(
            NewInvocation {
                id: start.invocation.clone(),
                fingerprint,
                client,
                epoch,
                thread: thread.as_str().to_owned(),
                owned,
                input,
                lease,
            },
            &self.areas.connections,
        );
        Ok(started_reply(InvocationStarted {
            invocation: start.invocation.clone(),
            thread,
            run: workbench_protocol::ids::RunId::new(run.to_string())
                .map_err(|_| reject_error(RejectKind::Failed, "invalid run"))?,
            ownership,
            already_started: false,
        }))
    }

    fn end_invocation(
        &mut self,
        end: &EndInvocation,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let client = invocation_client(provenance)?;
        let ended = self
            .service
            .invocations()
            .end(&end.invocation, &client)
            .map_err(invocation_error)?;
        Ok(CommandReply::with_result(CommandResult::Execution(
            ExecutionResult::InvocationEnded(ended),
        )))
    }
}

fn invocation_error(error: InvocationError) -> WorkbenchError {
    match error {
        InvocationError::NotFound => reject_error(RejectKind::NotFound, "unknown invocation"),
        InvocationError::NotOwner => WorkbenchError::new(
            ErrorCode::PermissionDenied,
            "the invocation belongs to another client",
            RetryAdvice::Never,
        ),
        InvocationError::StopFailed => WorkbenchError::new(
            ErrorCode::Internal,
            "the run could not be stopped; the invocation is still running",
            RetryAdvice::SameRequest,
        ),
    }
}

impl CommandHost {
    /// The plain run listing is read from the projection; the detail queries
    /// (roots and children, one run, goal and ledger) are answered by the
    /// execution area, which composes live and persisted runs. An invocation is
    /// read only by the client that started it.
    pub(in crate::ingress) fn execution_query(
        &self,
        query: &workbench_protocol::queries::Query,
        namespace_query: &workbench_protocol::namespaces::ExecutionQuery,
        provenance: &Provenance,
    ) -> Result<workbench_protocol::queries::QueryReply, WorkbenchError> {
        match namespace_query {
            workbench_protocol::namespaces::ExecutionQuery::ListRuns(_)
            | workbench_protocol::namespaces::ExecutionQuery::ReadShellOutput(_) => {
                self.projection.query(query)
            }
            workbench_protocol::namespaces::ExecutionQuery::GetInvocation(get) => {
                let client = invocation_client(provenance)?;
                let view = self
                    .service
                    .invocations()
                    .view(&get.invocation, &client)
                    .map_err(invocation_error)?;
                Ok(QueryReply::Execution(ExecutionReply::Invocation(Box::new(
                    view,
                ))))
            }
            _ => self.areas.execution.query(query, namespace_query),
        }
    }
}
