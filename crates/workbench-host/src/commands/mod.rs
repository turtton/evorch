//! The command services: what a thread-targeting request does to the runtime.
//!
//! [`CommandService`] holds the host-side state that the commands share (which
//! root run a thread is bound to, which stops this host issued, the ownership
//! permits it checked) and executes [`HostCommand`]s against the runtime. It is
//! the one implementation behind every ingress: the desktop GUI adapter and
//! the protocol handler ([`crate::ingress`]) both call [`CommandService::submit`].
//!
//! Nothing here reaches a GUI or client crate, and nothing here puts client
//! provenance into a model prompt (the prompt cache contract): provenance is
//! kept beside the command in [`crate::receipts`], not inside the run config.

mod chat;
mod compaction;
mod goal;
mod question;
mod stop;
mod types;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use event_bus::{Event, EventBus};
use runtime::{AgentRuntime, Role, RunConfig, RunId};

pub use goal::render_entry_prompt;
pub use types::{
    ChatInput, ComposerRole, ContinueInput, GoalInput, HostCommand, HostEvent, NoticeOutcome,
    PacketReference, ReferenceKind, RejectKind,
};

/// Per-thread state shared by the commands.
///
/// The fields are public so the in-process adapter and its tests can seed and
/// inspect bindings the way the desktop sidebar restores them; the commands are
/// the only code that changes them while a host runs.
#[derive(Default)]
pub struct ThreadBindings {
    pub chat_runs: BTreeMap<String, RunId>,
    pub goal_runs: BTreeMap<String, RunId>,
    pub running_children: BTreeMap<String, usize>,
    /// A second press must escalate before the async root phase changes.
    pub stop_marked: BTreeSet<String>,
    /// Retain resume eligibility even after stage two clears `stop_marked`.
    pub stopped_by_us: BTreeSet<String>,
    pub goal_projects: BTreeMap<String, String>,
    pub chat_permits: BTreeMap<String, runtime::ownership::OwnerPermit>,
    /// Project each thread last submitted from, for host follow-ups that carry none.
    pub thread_roots: BTreeMap<String, PathBuf>,
}

impl ThreadBindings {
    /// The root run commands for `thread` act on: the chat root, else the goal root.
    pub fn root_of(&self, thread: &str) -> Option<RunId> {
        self.chat_runs
            .get(thread)
            .or_else(|| self.goal_runs.get(thread))
            .copied()
    }

    fn rebind_conversation_root(&mut self, thread: &str, run: RunId) {
        self.chat_runs
            .retain(|owner, root| owner == thread || *root != run);
        self.goal_runs
            .retain(|owner, root| owner == thread || *root != run);
    }
}

/// Executes [`HostCommand`]s against one runtime.
pub struct CommandService {
    event_bus: Option<Arc<EventBus>>,
    team_writer: Option<storage::StorageHandle>,
    memory_config: Option<storage::StorageConfig>,
    runtime: AgentRuntime,
    shell_cwd: Option<PathBuf>,
    handle: tokio::runtime::Handle,
    pub bindings: ThreadBindings,
    /// Set while serving a runtime-requested resume, which is not user input.
    automatic_resume: bool,
    ownership: Option<Arc<runtime::ownership::OwnerHost>>,
    compaction_jobs: BTreeMap<String, compaction::CompactionJob>,
}

impl CommandService {
    /// A service over `runtime`, spawning its async work on `handle`.
    pub fn new(runtime: AgentRuntime, handle: tokio::runtime::Handle) -> Self {
        Self {
            event_bus: None,
            team_writer: None,
            memory_config: None,
            runtime,
            shell_cwd: None,
            handle,
            bindings: ThreadBindings::default(),
            automatic_resume: false,
            ownership: None,
            compaction_jobs: BTreeMap::new(),
        }
    }

    pub fn with_web_tools_enabled(mut self, enabled: bool) -> Self {
        self.set_web_tools_enabled(enabled);
        self
    }

    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.set_event_bus(event_bus);
        self
    }

    pub fn with_ownership(mut self, ownership: Arc<runtime::ownership::OwnerHost>) -> Self {
        self.ownership = Some(ownership);
        self
    }

    pub fn with_memory_storage(mut self, config: storage::StorageConfig) -> Self {
        self.set_memory_storage(config);
        self
    }

    pub fn with_team_writer(mut self, writer: storage::StorageHandle) -> Self {
        self.set_team_writer(writer);
        self
    }

    pub fn set_web_tools_enabled(&mut self, enabled: bool) {
        self.runtime.set_web_tools_enabled(enabled);
    }

    pub fn set_event_bus(&mut self, event_bus: Arc<EventBus>) {
        self.event_bus = Some(event_bus);
    }

    pub fn set_memory_storage(&mut self, config: storage::StorageConfig) {
        self.memory_config = Some(config);
    }

    pub fn set_team_writer(&mut self, writer: storage::StorageHandle) {
        self.team_writer = Some(writer);
    }

    pub fn runtime(&self) -> &AgentRuntime {
        &self.runtime
    }

    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.handle
    }

    pub fn ownership(&self) -> Option<&Arc<runtime::ownership::OwnerHost>> {
        self.ownership.as_ref()
    }

    pub fn set_ownership(&mut self, ownership: Option<Arc<runtime::ownership::OwnerHost>>) {
        self.ownership = ownership;
    }

    /// Whether a compaction job started by [`HostCommand::CompactChat`] is still
    /// tracked, so a caller knows [`Self::poll`] has a result to collect.
    pub fn has_background_work(&self) -> bool {
        !self.compaction_jobs.is_empty()
    }

    /// The compaction task of `thread`, for tests that must await it.
    #[doc(hidden)]
    pub fn compaction_task(&mut self, thread: &str) -> Option<&mut tokio::task::JoinHandle<()>> {
        self.compaction_jobs
            .get_mut(thread)
            .map(|job| &mut job.task)
    }

    /// A background run that belongs to no thread.
    pub fn start_background_run(&self, text: String) -> RunId {
        let _guard = self.handle.enter();
        self.runtime.delegate_background(
            Role::Worker,
            text,
            RunConfig {
                interactive: false,
                keep_alive: false,
                ..RunConfig::default()
            },
        )
    }

    /// Lessons are keyed by the slug post-run learning stores them under, not the sidebar id.
    pub fn capture_memory(&self, root: Option<&Path>) -> Option<runtime::memory::MemoryBoundary> {
        let config = self.memory_config.as_ref()?;
        let root = root.or(self.shell_cwd.as_deref())?;
        let project = self
            .runtime
            .project_slug(root)
            .unwrap_or_else(|| runtime::repository::derive_repo_slug(root));
        runtime::memory::MemoryBoundary::capture(config, &project)
            .inspect_err(|error| tracing::warn!(%error, %project, "memory read failed"))
            .ok()
    }

    /// Whether a stop this host issued left a chat whose saved context is gone, so the
    /// next message may start it over instead of failing.
    pub fn can_restart_stopped(&self, thread: &str, error: &runtime::RuntimeError) -> bool {
        (self.bindings.stop_marked.contains(thread) || self.bindings.stopped_by_us.contains(thread))
            && matches!(
                error,
                runtime::RuntimeError::RunRestoreFailed {
                    reason: runtime::RunRestoreFailure::MissingContext
                        | runtime::RunRestoreFailure::StorageNotConfigured,
                    ..
                }
            )
    }

    /// Withdraw an accepted input before it enters the live agent context.
    pub fn withdraw_follow_up(
        &mut self,
        thread: &str,
        prompt_id: &str,
    ) -> Result<(), runtime::RuntimeError> {
        let run = self
            .bindings
            .root_of(thread)
            .ok_or(runtime::FollowUpWithdrawalError::Unknown)?;
        self.runtime.withdraw_follow_up(run, prompt_id)
    }

    /// Runtime receipt observation, not an acknowledgement of command acceptance.
    pub fn follow_up_status(&self, thread: &str) -> Option<runtime::FollowUpStatus> {
        let run = self.bindings.root_of(thread)?;
        self.runtime.follow_up_status(run).ok()
    }

    /// Live count of the root's running descendants, remembered for the stopped banner.
    pub fn running_children(&mut self, thread: &str) -> Option<usize> {
        let run = self.bindings.root_of(thread)?;
        let count = self.runtime.live_descendants(run).len();
        self.bindings.running_children.insert(thread.into(), count);
        Some(count)
    }

    pub fn observe_lifecycle(&mut self, event: &Event) {
        if matches!(event.kind, event_bus::EventKind::Lifecycle(_)) {
            // Refresh all bound roots: a stopped/completed child is no longer in
            // live_descendants, so filtering by the live set would miss it.
            let threads: BTreeSet<_> = self
                .bindings
                .chat_runs
                .keys()
                .chain(self.bindings.goal_runs.keys())
                .cloned()
                .collect();
            for thread in threads {
                self.running_children(&thread);
            }
        }
    }

    /// Restore procedure data only; this cannot authorize or wake a root run.
    pub fn bind_thread_todo(&mut self, snapshot: &event_bus::ThreadTodoSnapshot) {
        if let Err(error) = self.runtime.restore_thread_todo(snapshot.clone()) {
            tracing::warn!(%error, "failed to restore thread procedures");
        }
    }

    pub fn bind_thread_goal(&mut self, snapshot: &event_bus::ThreadGoalSnapshot, project: &str) {
        self.bindings
            .goal_projects
            .insert(snapshot.thread_id.clone(), project.into());
        if let Ok(id) = snapshot.root_run_id.parse::<RunId>() {
            self.bindings
                .rebind_conversation_root(&snapshot.thread_id, id);
            self.bindings
                .chat_runs
                .insert(snapshot.thread_id.clone(), id);
        }
    }

    /// Point a lane thread at the root run the runtime itself started, without spawning
    /// work. Stop and follow-ups then reach it like any other conversation root; every
    /// ownership check still applies to the command that uses the binding.
    pub fn bind_lane_root(&mut self, thread: &str, run: &str) {
        // The runtime decides what a lane root is; a name or a child never qualifies.
        if let Ok(id) = run.parse::<RunId>()
            && self.runtime.is_lane_root(thread, id)
        {
            self.bindings.rebind_conversation_root(thread, id);
            self.bindings.chat_runs.insert(thread.into(), id);
        }
    }

    /// Rebind a thread to a root run (escalation handoff), without spawning work.
    pub fn bind_goal_context(&mut self, thread: &str, project: &str, run: &str) {
        if let Ok(id) = run.parse::<RunId>() {
            self.bindings.rebind_conversation_root(thread, id);
            self.bindings.goal_runs.insert(thread.into(), id);
            self.bindings
                .goal_projects
                .insert(thread.into(), project.into());
        }
    }

    pub fn set_default_cwd(&mut self, cwd: Option<PathBuf>) -> Result<(), String> {
        // cwd 未指定時は起動済み executor を維持し、不要な sandbox 構築を避ける。
        let Some(root) = cwd else {
            self.shell_cwd = None;
            return Ok(());
        };
        if self.shell_cwd.as_ref() == Some(&root) {
            return Ok(());
        }
        self.runtime
            .set_project_root(root.clone())
            .map_err(|error| error.to_string())?;
        self.shell_cwd = Some(root);
        Ok(())
    }

    /// Collect the results of finished background jobs (saved-context compaction).
    pub fn poll(&mut self) -> Vec<HostEvent> {
        self.poll_compactions()
    }

    /// Execute a command for the thread's current owner.
    ///
    /// With an ownership host configured, a command that targets a thread needs
    /// that host to own the thread right now; a command that does not is refused
    /// with [`RejectKind::Fenced`] before it reaches the runtime.
    pub fn submit(&mut self, command: HostCommand) -> Vec<HostEvent> {
        let permit = if let Some(host) = &self.ownership {
            match command
                .thread()
                .map(|thread| host.owned_permit(thread))
                .transpose()
            {
                Ok(permit) => permit,
                Err(error) => {
                    return vec![HostEvent::rejected(RejectKind::Fenced, error.to_string())];
                }
            }
        } else {
            None
        };
        self.submit_authorized(command, permit)
    }

    /// Send a chat under a permit the caller already holds, after checking that
    /// it still is the thread's current ownership.
    pub fn submit_chat_with_permit(
        &mut self,
        chat: ChatInput,
        permit: runtime::ownership::OwnerPermit,
    ) -> Vec<HostEvent> {
        let validation = (|| {
            if permit.thread_id != chat.thread_id {
                return Err(runtime::ownership::OwnershipError::Fenced.into());
            }
            if let Some(host) = &self.ownership {
                let current = host.owned_permit(&chat.thread_id)?;
                if current.registry_path != permit.registry_path
                    || current.lease.owner_id != permit.lease.owner_id
                    || current.lease.generation != permit.lease.generation
                {
                    return Err(runtime::ownership::OwnershipError::Fenced.into());
                }
            }
            permit.validate_generation()
        })();
        match validation {
            Ok(()) => self.submit_authorized(HostCommand::SendChat(chat), Some(permit)),
            Err(error) => vec![HostEvent::chat_rejected(
                chat.thread_id,
                RejectKind::Fenced,
                error.to_string(),
            )],
        }
    }

    /// Run a command under a permit the caller already holds (or none without an
    /// ownership host), skipping the lookup [`Self::submit`] does.
    pub fn submit_authorized(
        &mut self,
        command: HostCommand,
        permit: Option<runtime::ownership::OwnerPermit>,
    ) -> Vec<HostEvent> {
        if let Some(thread) = Self::compaction_excluded_thread(&command)
            && self.compaction_jobs.contains_key(thread)
        {
            let reason = "Context compaction is in progress; try again after it completes";
            if matches!(command, HostCommand::SendChat(_)) {
                return vec![HostEvent::chat_rejected(thread, RejectKind::Busy, reason)];
            }
            return vec![HostEvent::notice(thread, reason)];
        }
        match command {
            HostCommand::AnswerUserQuestion {
                thread_id,
                question_id,
                answer,
            } => self.answer_user_question(thread_id, question_id, answer, permit),
            HostCommand::SetWebToolsEnabled { enabled } => {
                self.runtime.set_web_tools_enabled(enabled);
                Vec::new()
            }
            HostCommand::DecideToolApproval { call_id, approved } => {
                if let Some(bus) = &self.event_bus {
                    bus.emit(Event::new(event_bus::ToolEvent::ApprovalResolved {
                        call_id,
                        approved,
                    }));
                }
                Vec::new()
            }
            HostCommand::CompactChat { thread_id } => self.compact_chat(thread_id, permit),
            HostCommand::DeliverFollowUpsNextTurn { thread_id } => {
                self.deliver_follow_ups_next_turn(thread_id)
            }
            HostCommand::StopRun { thread_id, run_id } => self.stop_run(thread_id, run_id, permit),
            HostCommand::StopChat { thread_id } => self.stop_chat(thread_id),
            HostCommand::SubmitGoal(submission) => self.submit_goal(submission, permit),
            HostCommand::SetGoalReview {
                thread_id,
                goal_id,
                enabled,
            } => self.goal_control(&thread_id, permit, |runtime| {
                runtime.set_goal_review(&thread_id, &goal_id, enabled)
            }),
            HostCommand::SetGoalChecksPaused {
                thread_id,
                goal_id,
                paused,
            } => self.goal_control(&thread_id, permit, |runtime| {
                runtime.set_goal_checks_paused(&thread_id, &goal_id, paused)
            }),
            HostCommand::SendChat(submission) => self.submit_chat(submission, permit, false),
            HostCommand::ContinueChat(continuation) => {
                self.automatic_resume = continuation.automatic;
                let events = self.submit_chat(
                    ChatInput {
                        submission_id: String::new(),
                        fork_seed: None,
                        project_root: continuation.project_root,
                        thread_id: continuation.thread_id,
                        composer_role: continuation.composer_role,
                        model_preference: continuation.model_preference,
                        images: Vec::new(),
                        // The resume_only path never submits this placeholder as input.
                        text: String::new(),
                        skills: Vec::new(),
                    },
                    permit,
                    true,
                );
                self.automatic_resume = false;
                events
            }
        }
    }

    /// The thread whose running compaction excludes this command.
    fn compaction_excluded_thread(command: &HostCommand) -> Option<&str> {
        match command {
            HostCommand::CompactChat { thread_id }
            | HostCommand::StopChat { thread_id }
            | HostCommand::StopRun { thread_id, .. } => Some(thread_id),
            HostCommand::ContinueChat(chat) => Some(&chat.thread_id),
            HostCommand::SendChat(chat) => Some(&chat.thread_id),
            _ => None,
        }
    }

    fn deliver_follow_ups_next_turn(&mut self, thread_id: String) -> Vec<HostEvent> {
        let Some(run) = self.bindings.root_of(&thread_id) else {
            return vec![HostEvent::chat_rejected(
                thread_id,
                RejectKind::NotFound,
                "No conversation to deliver follow-ups to",
            )];
        };
        match self.runtime.deliver_follow_ups_next_turn(run) {
            Ok(()) => Vec::new(),
            Err(error) => vec![HostEvent::chat_rejected(
                thread_id,
                RejectKind::Failed,
                error.to_string(),
            )],
        }
    }
}
