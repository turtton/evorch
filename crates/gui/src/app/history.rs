use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct UserMessage {
    /// The submission ID; empty in files saved before submissions had one.
    #[serde(default)]
    pub id: String,
    pub thread_id: String,
    pub text: String,
    /// The expanded prompt the agent receives; empty in older files.
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub image_count: usize,
    #[serde(default)]
    pub resent_from: Option<String>,
    pub at: std::time::SystemTime,
    #[serde(default)]
    pub status: DeliveryStatus,
    /// The message started a run rather than waiting in a live run's inbox, so
    /// it shows as sent while its receipt is still `Queued`. Persisted so a
    /// restart before delivery restores the same presentation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starts_run: bool,
    /// What the user did about a message that never reached the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
}

/// How far a composer message got towards the agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum DeliveryStatus {
    /// Handed to the runtime; a busy run may still hold it in its inbox.
    Queued,
    /// In the conversation. Files from before statuses existed only kept
    /// messages once they were sent, so they load as delivered.
    #[default]
    Delivered,
    /// The runtime refused it, so the agent never received it.
    Rejected { reason: String },
    /// Accepted, but the run ended before reading it.
    Dropped,
    /// No durable snapshot proves whether the model read this input.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Resolution {
    /// Its text went back into the composer.
    MovedToComposer {
        draft_id: String,
    },
    Resent {
        new_id: String,
    },
    Dismissed,
}

impl UserMessage {
    /// A committed context snapshot makes a sent transcript bubble, and so does
    /// starting a run.
    pub(super) fn shown_as_sent(&self) -> bool {
        self.status == DeliveryStatus::Delivered || self.sent_while_queued()
    }

    /// A message that started its run: shown as sent until a receipt refutes it.
    pub(super) fn sent_while_queued(&self) -> bool {
        self.starts_run && self.status == DeliveryStatus::Queued && self.resolution.is_none()
    }

    /// A message that never reached the agent and that the user has not
    /// restored or dismissed yet.
    pub(super) fn awaits_user(&self) -> bool {
        self.resolution.is_none()
            && matches!(
                self.status,
                DeliveryStatus::Rejected { .. } | DeliveryStatus::Dropped | DeliveryStatus::Unknown
            )
    }

    pub(super) fn undelivered_row(&self) -> crate::model::composer::QueuedFollowUp {
        crate::model::composer::QueuedFollowUp {
            submission_id: self.id.clone(),
            text: self.text.clone(),
            prompt: self.prompt.clone(),
            undelivered: true,
            rejection: match &self.status {
                DeliveryStatus::Rejected { reason } => Some(format!("Not sent: {reason}")),
                DeliveryStatus::Unknown => Some(
                    "Delivery unknown — resending may duplicate it. Check before resending".into(),
                ),
                _ => None,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct ComposerDraft {
    pub draft_id: String,
    pub thread_id: String,
    pub text: String,
    pub origin: String,
    pub images_needed: usize,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct HistorySidebar {
    #[serde(flatten)]
    pub sidebar: workspace_ui::SidebarState,
    #[serde(default)]
    pub user_messages: Vec<UserMessage>,
    #[serde(default)]
    pub composer_drafts: Vec<ComposerDraft>,
}

#[derive(Serialize)]
pub(super) struct HistorySidebarRef<'a> {
    #[serde(flatten)]
    pub sidebar: &'a workspace_ui::SidebarState,
    pub user_messages: &'a [UserMessage],
    pub composer_drafts: &'a [ComposerDraft],
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Saves the sidebar, its messages and drafts to the file other GUI
    /// processes share. When another process saved since this one last read or
    /// wrote it, its changes are merged in so neither side's edits are lost;
    /// they reach this process's state at the next import.
    pub(super) fn write_history_sidebar(
        &self,
        path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.sidebar.validate()?;
        let saved = HistorySidebarRef {
            sidebar: &self.sidebar,
            user_messages: &self.history,
            composer_drafts: &self.composer_drafts,
        };
        let local = serde_json::to_vec_pretty(&saved)?;
        self.shared_sidebar_write(path, local)?;
        #[cfg(test)]
        self.sidebar_write_count
            .set(self.sidebar_write_count.get() + 1);
        Ok(())
    }

    pub fn restore_history(&mut self, db: &storage::Database) -> Result<(), storage::StorageError> {
        self.restore_history_inner(db, None)
    }

    /// Reconcile orphaned phases only when the owner registry can be inspected.
    pub fn restore_history_with_ownership(
        &mut self,
        db: &storage::Database,
        registry_path: &std::path::Path,
    ) -> Result<(), storage::StorageError> {
        self.restore_history_inner(db, Some(registry_path))
    }

    fn restore_history_inner(
        &mut self,
        db: &storage::Database,
        registry_path: Option<&std::path::Path>,
    ) -> Result<(), storage::StorageError> {
        // Two streamed passes keep a long history out of memory: the first
        // finds where each run last waited, the second replays.
        let mut restored_tasks = crate::model::tasks::TaskRestore::default();
        // A legacy event subscriber could drop stream deltas under load. The
        // context snapshot still contains the complete accepted response.
        // Repair the final waiting turn at its original position in the replay.
        let mut last_waiting = std::collections::BTreeMap::new();
        let mut completed_at_waiting = std::collections::BTreeMap::new();
        let mut completed_since_request = std::collections::BTreeMap::new();
        let mut replay_end = None;
        // A response too large for one row is stored as parts; rebuild it.
        let mut parts = event_bus::MessagePartAssembler::new();
        db.for_each_event_ordered(|stored| {
            let Some(assembled) = parts.accept(&stored.event) else {
                return;
            };
            let event: &event_bus::Event = &assembled;
            self.tasks.restore_event(&mut restored_tasks, event);
            replay_end = Some(event.meta.wall_clock);
            match &event.kind {
                event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestStarted {
                    run_id: Some(run_id),
                    ..
                }) => {
                    completed_since_request.insert(run_id.clone(), false);
                }
                event_bus::EventKind::Message(event_bus::MessageEvent::MessageCompleted {
                    run_id,
                    ..
                }) => {
                    completed_since_request.insert(run_id.clone(), true);
                }
                event_bus::EventKind::Lifecycle(
                    event_bus::LifecycleEvent::AgentRunStateChanged {
                        run_id,
                        to: event_bus::AgentRunPhase::Waiting,
                        ..
                    },
                ) => {
                    last_waiting.insert(run_id.clone(), stored.id);
                    completed_at_waiting.insert(
                        run_id.clone(),
                        completed_since_request
                            .get(run_id)
                            .copied()
                            .unwrap_or(false),
                    );
                }
                _ => {}
            }
        })?;
        self.tasks.finish_restore(restored_tasks);
        self.user_questions = db
            .pending_user_questions()?
            .into_iter()
            .map(|q| (q.id.clone(), q))
            .collect();
        if let Some(path) = &self.sidebar_path {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let saved: HistorySidebar = serde_json::from_slice(&bytes)
                        .map_err(|error| storage::StorageError::Serialization(error.to_string()))?;
                    self.history = saved.user_messages;
                    self.composer_drafts = saved.composer_drafts;
                    self.shared_sidebar_restored();
                    let ids: Vec<_> = self
                        .history
                        .iter()
                        .map(|message| message.id.clone())
                        .collect();
                    let receipts = db.host_input_receipts(&ids)?;
                    self.apply_receipts(&receipts, true);
                    self.restore_undelivered_rows();
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage::StorageError::Serialization(error.to_string())),
            }
        }
        // The first root chat run owns a thread's role. Repair both missing
        // roles and values overwritten by role cycling in older sidebar files.
        let mut inferred_role = false;
        for thread in &mut self.sidebar.threads {
            if thread.escalation_source_run_id.is_some() {
                continue;
            }
            for run_id in &thread.run_ids {
                let Some(record) = db.run_context(run_id)? else {
                    continue;
                };
                if record.parent_run_id.is_some()
                    || record.name != format!("chat:{}:{}", record.role, thread.id)
                {
                    continue;
                }
                let original_role = match record.role.as_str() {
                    "Worker" => Some(workspace_ui::ThreadChatRole::Worker),
                    "Orchestrator" => Some(workspace_ui::ThreadChatRole::Orchestrator),
                    _ => None,
                };
                if let Some(role) = original_role {
                    if thread.chat_role != Some(role) {
                        thread.chat_role = Some(role);
                        inferred_role = true;
                    }
                    break;
                }
            }
        }
        if let Some(thread) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
        {
            self.composer.restore_thread_role(thread);
        }
        if inferred_role {
            self.save_sidebar();
        }
        self.transcripts = crate::model::transcript_registry::TranscriptRegistry::new();
        for thread in &self.sidebar.threads {
            for run in &thread.run_ids {
                self.transcripts.bind_run(run, &thread.id.to_string());
            }
        }
        let mut completed_snapshots = std::collections::BTreeMap::new();
        for thread in &self.sidebar.threads {
            for run_id in &thread.run_ids {
                if !last_waiting.contains_key(run_id) {
                    continue;
                }
                let Some(record) = db.run_context(run_id)? else {
                    continue;
                };
                if record.terminal_phase != "Checkpoint" {
                    continue;
                }
                let Ok(messages) =
                    serde_json::from_str::<Vec<providers::Message>>(&record.messages_json)
                else {
                    continue;
                };
                let Some(last) = messages.last().filter(|message| {
                    message.role == providers::Role::Assistant
                        && !message
                            .content
                            .iter()
                            .any(|block| matches!(block, providers::ContentBlock::ToolUse { .. }))
                }) else {
                    continue;
                };
                let text: String = last
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        providers::ContentBlock::Text { text } => Some(text.as_str()),
                        providers::ContentBlock::Compaction { .. } => {
                            tracing::warn!("テキスト抽出では compaction block をスキップします");
                            None
                        }
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    completed_snapshots.insert(run_id.clone(), text);
                }
            }
        }
        self.seed_branches(None);
        let mut messages = self.history.clone();
        messages.sort_by_key(|message| message.at);
        let mut messages = messages.into_iter().peekable();
        self.telemetry = crate::model::telemetry::TelemetryOverlay::new();
        let replay_now = std::time::Instant::now();
        let mut replayed_to = 0;
        let mut parts = event_bus::MessagePartAssembler::new();
        db.for_each_event_ordered(|stored| {
            replayed_to = replayed_to.max(stored.id);
            let Some(assembled) = parts.accept(&stored.event) else {
                return;
            };
            let event: &event_bus::Event = &assembled;
            while messages
                .peek()
                .is_some_and(|message| message.at <= event.meta.wall_clock)
            {
                if let Some(message) = messages.next() {
                    self.restore_user_message(message);
                }
            }
            let at = replay_end
                .and_then(|end| end.duration_since(event.meta.wall_clock).ok())
                .and_then(|age| replay_now.checked_sub(age))
                .unwrap_or(replay_now);
            self.telemetry.apply_event_at(event, at);
            self.transcripts.select_thread(None);
            self.apply_conversation_event(event);
            self.seed_system_notification(event);
            if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) = &event.kind
            {
                self.seed_branches(Some(&workspace_ui::ForkPoint {
                    run_id: run_id.clone(),
                    context_len: *context_len,
                }));
            }
            self.bind_goal_event(event);
            if let event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to, .. },
            ) = &event.kind
            {
                // In particular, Stopped survives replay as resumable, not failed/idle.
                self.phases.insert(run_id.clone(), super::frame::phase(*to));
            }
            if let event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, .. },
            ) = &event.kind
                && last_waiting.get(run_id) == Some(&stored.id)
                && !completed_at_waiting.get(run_id).copied().unwrap_or(false)
                && let Some(text) = completed_snapshots.get(run_id)
            {
                self.apply_conversation_event(&event_bus::Event::new(
                    event_bus::MessageEvent::MessageCompleted {
                        run_id: run_id.clone(),
                        text: text.clone(),
                    },
                ));
            }
        })?;
        self.note_replayed_event(replayed_to);
        self.note_restored_active_runs();
        self.reconcile_restored_phases(db, registry_path);
        let ids: Vec<_> = self
            .history
            .iter()
            .map(|message| message.id.clone())
            .collect();
        self.apply_receipts(&db.host_input_receipts(&ids)?, true);
        self.rebuild_delivery_rows();
        for message in messages {
            self.restore_user_message(message);
        }
        self.transcripts.finish_history();
        self.seed_pending_question_notifications();
        self.telemetry.finish_history();
        self.ledger.load_all(db.run_ledger_all()?);
        self.transcripts
            .select_thread(self.sidebar.active_thread.as_ref().map(ToString::to_string));
        self.refresh_active_thread_workspace();
        self.refresh_draft_banner();
        Ok(())
    }

    fn reconcile_restored_phases(
        &mut self,
        db: &storage::Database,
        registry_path: Option<&std::path::Path>,
    ) {
        use workspace_ui::ThreadRunPhase;
        // An absent registry or an expired lease is not proof of an orphan.
        let Some(path) = registry_path else { return };
        let (registry, owners) = match runtime::ownership::Registry::open_readonly(path)
            .and_then(|registry| registry.list().map(|owners| (registry, owners)))
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "history phase reconciliation skipped: ownership unknown");
                return;
            }
        };
        // Legacy active turns cannot be attributed to a run: protect all runs.
        if owners
            .iter()
            .any(|owner| owner.active_turn && owner.active_runs.is_empty())
        {
            tracing::warn!("history phase reconciliation skipped: legacy active turn");
            return;
        }
        // Checkpoint removes active_runs after each tool round even while the
        // run stays Running. Protect its thread for the owner's full lifetime,
        // including expired leases and endpoint failures of unknown cause.
        let Some(root) = path.parent() else { return };
        let mut liveness = std::collections::BTreeMap::new();
        let mut protected_threads: std::collections::BTreeSet<_> = owners
            .iter()
            .filter(|owner| {
                owner.state != runtime::ownership::OwnerState::Released
                    && *liveness.entry(&owner.lease.owner_id).or_insert_with(|| {
                        runtime::ownership::ipc::owner_may_be_live(root, &owner.lease.owner_id)
                    })
            })
            .map(|owner| owner.thread_id.clone())
            .collect();
        // Do not hold a registry transaction across the socket probes. If a
        // claim, release, or new turn raced with them, keep the replayed phase.
        let current_owners = match registry.list() {
            Ok(owners) => owners,
            Err(error) => {
                tracing::warn!(%error, "history phase reconciliation skipped: ownership changed or unreadable");
                return;
            }
        };
        let before: std::collections::BTreeMap<_, _> = owners
            .iter()
            .map(|owner| (&owner.thread_id, owner))
            .collect();
        let after: std::collections::BTreeMap<_, _> = current_owners
            .iter()
            .map(|owner| (&owner.thread_id, owner))
            .collect();
        for thread in before.keys().chain(after.keys()) {
            if before.get(thread) != after.get(thread) {
                protected_threads.insert((*thread).clone());
            }
        }
        if current_owners
            .iter()
            .any(|owner| owner.active_turn && owner.active_runs.is_empty())
        {
            return;
        }
        let running_calls = self.transcripts.running_tool_runs();
        let protected_runs: std::collections::BTreeSet<_> = self
            .phases
            .keys()
            .chain(&running_calls)
            .filter(|run| {
                self.thread_for_run(run)
                    .is_none_or(|thread| protected_threads.contains(&thread))
            })
            .cloned()
            .collect();
        for (run_id, phase) in &mut self.phases {
            if !matches!(
                phase,
                ThreadRunPhase::Running | ThreadRunPhase::Pending | ThreadRunPhase::Waiting
            ) || protected_runs.contains(run_id)
                || owners
                    .iter()
                    .chain(&current_owners)
                    .any(|owner| owner.active_runs.contains(run_id))
            {
                continue;
            }
            let record = match db.run_context(run_id) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    tracing::warn!(%run_id, "history phase reconciliation skipped: snapshot missing");
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%run_id, %error, "history phase reconciliation skipped: snapshot unreadable");
                    continue;
                }
            };
            let restored = match record.terminal_phase.as_str() {
                "Done" => ThreadRunPhase::Done,
                "Error" => ThreadRunPhase::Error,
                "Stopped" | "Checkpoint" => ThreadRunPhase::Stopped,
                _ => continue,
            };
            tracing::warn!(%run_id, from = ?phase, to = ?restored, "reconciled orphaned history phase");
            *phase = restored;
            self.transcripts.interrupt_run_tools(run_id);
            if let Err(error) = self.sink.mark_unreachable_host_inputs(run_id) {
                tracing::warn!(%run_id, %error, "orphan receipt reconciliation failed");
            }
        }
        // An owner that exited mid-call leaves the run in its active_runs.
        // Only a probe that proved the owner dead, of a record that did not
        // change during the probes, shows the run is no longer executing.
        let executing = |run: &String| {
            owners.iter().chain(&current_owners).any(|owner| {
                owner.active_runs.contains(run)
                    && !(liveness.get(&owner.lease.owner_id) == Some(&false)
                        && before.get(&owner.thread_id) == after.get(&owner.thread_id))
            })
        };
        // Receipt reconciliation also covers runs whose last event already
        // looked terminal (a failed terminal transaction leaves Accepted rows).
        let ids: Vec<_> = self
            .history
            .iter()
            .map(|message| message.id.clone())
            .collect();
        let receipts = match db.host_input_receipts(&ids) {
            Ok(receipts) => receipts,
            Err(error) => {
                tracing::warn!(%error, "orphan receipt lookup failed");
                Vec::new()
            }
        };
        for (thread, runs) in self.history_run_candidates(&receipts) {
            if protected_threads.contains(&thread) {
                continue;
            }
            for run in runs {
                if !executing(&run)
                    && !self.receipt_run_is_live(&run)
                    && let Err(error) = self.sink.mark_unreachable_host_inputs(&run)
                {
                    tracing::warn!(%run, %error, "orphan receipt reconciliation failed");
                }
            }
        }
        // No live owner can be executing these runs, so their calls will never
        // report a result. This includes runs whose phase stays as replayed
        // because their snapshot or every phase event is missing.
        for run_id in &running_calls {
            if !protected_runs.contains(run_id) && !executing(run_id) {
                self.transcripts.interrupt_run_tools(run_id);
            }
        }
    }

    /// This process has just claimed `thread_id`. A run executes only under
    /// its thread's owner, so a run of that thread unknown to this process's
    /// runtime cannot be running anywhere: its terminal event was lost (for
    /// example while the previous owner shut down) or its process ended.
    /// History reconciliation left it alone because the previous owner could
    /// still have been live; now it is shown as Stopped, like an orphaned
    /// checkpoint, instead of keeping the thread running forever.
    #[cfg(test)]
    pub(super) fn reconcile_claimed_thread(&mut self, thread_id: &str) {
        self.receipt_reconciliation.last_repair = None;
        let mut receipts = self
            .read_receipts_if_due(std::time::Instant::now())
            .unwrap_or_default();
        self.settle_claimed_receipts(thread_id, &mut receipts);
        let changed = self.apply_receipts(&receipts, false);
        self.finish_receipt_reconciliation(changed);
    }

    pub(super) fn settle_claimed_receipts(
        &mut self,
        thread_id: &str,
        receipts: &mut [storage::HostInputReceipt],
    ) {
        use workspace_ui::ThreadRunPhase;
        let runs = self
            .history_run_candidates(receipts)
            .remove(thread_id)
            .unwrap_or_default();
        for run_id in &runs {
            if self.receipt_run_is_live(run_id) {
                continue;
            }
            match self.sink.mark_unreachable_host_inputs(run_id) {
                Err(error) => {
                    tracing::warn!(%run_id, %error, "claimed receipt reconciliation failed");
                }
                Ok(changed) => {
                    // Only storage-confirmed transitions may change the projection.
                    // Reuse this batch rather than querying storage again.
                    let changed: std::collections::HashSet<_> = changed.iter().collect();
                    for receipt in receipts.iter_mut() {
                        if changed.contains(&receipt.prompt_id) {
                            receipt.status = storage::HostInputStatus::Unknown;
                        }
                    }
                }
            }
            let Some(phase) = self.phases.get_mut(run_id) else {
                continue;
            };
            if !matches!(
                phase,
                ThreadRunPhase::Running | ThreadRunPhase::Pending | ThreadRunPhase::Waiting
            ) {
                continue;
            }
            tracing::warn!(%run_id, %thread_id, from = ?phase, "reconciled an orphaned run of a claimed thread");
            *phase = ThreadRunPhase::Stopped;
            self.transcripts.interrupt_run_tools(run_id);
        }
    }

    fn receipt_run_is_live(&self, run: &str) -> bool {
        run.parse()
            .ok()
            .is_some_and(|run| self.tasks.host_input_run_is_live(run))
    }

    fn history_run_candidates(
        &self,
        receipts: &[storage::HostInputReceipt],
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        let mut candidates: std::collections::BTreeMap<_, std::collections::BTreeSet<_>> = self
            .sidebar
            .threads
            .iter()
            .map(|thread| {
                (
                    thread.id.to_string(),
                    thread.run_ids.iter().cloned().collect(),
                )
            })
            .collect();
        let receipts: std::collections::HashMap<_, _> = receipts
            .iter()
            .map(|receipt| (receipt.prompt_id.as_str(), receipt))
            .collect();
        // Accepted commits before ChatAccepted attaches the run to the sidebar.
        // The saved submission ID is sufficient to recover that crash window.
        for message in &self.history {
            if let Some(receipt) = receipts.get(message.id.as_str()) {
                candidates
                    .entry(message.thread_id.clone())
                    .or_default()
                    .insert(receipt.run_id.clone());
            }
        }
        candidates
    }

    pub(super) fn restore_user_message(&mut self, message: UserMessage) {
        let message = self
            .history
            .iter()
            .find(|saved| !message.id.is_empty() && saved.id == message.id)
            .cloned()
            .unwrap_or(message);
        if !message.shown_as_sent() {
            return;
        }
        self.transcripts
            .push_host_input(&message.thread_id, &message.id, message.text);
    }

    /// Messages that never reached the agent come back above the composer,
    /// not as sent bubbles, until the user restores or dismisses them.
    pub(super) fn restore_undelivered_rows(&mut self) {
        for message in self.history.iter().filter(|message| message.awaits_user()) {
            let rows = self
                .follow_up_queues
                .entry(message.thread_id.clone())
                .or_default();
            if !rows.iter().any(|row| row.submission_id == message.id) {
                rows.push(message.undelivered_row());
            }
        }
    }
}
