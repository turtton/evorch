use super::*;

impl AgentRuntime {
    pub(super) fn register_restored(
        &self,
        runs: &mut HashMap<RunId, RunEntry>,
        identity: (RunId, Option<RunId>, Role, RunConfig, RunId),
        restored: RestoredState,
    ) {
        let (run_id, parent, role, config, sender) = identity;
        let name = runs.get(&run_id).map_or_else(
            || config.name.clone().unwrap_or_else(|| role.name().into()),
            |entry| entry.name.clone(),
        );
        let model = runs.get(&run_id).map_or_else(
            || self.shared.model.selected_model(role),
            |entry| entry.model.clone(),
        );
        let (phase_tx, phase_rx) = watch::channel(AgentRunPhase::Pending);
        let (message_count_tx, message_count_rx) = watch::channel(restored.messages.len() + 1);
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (compact_tx, compact_rx) = watch::channel(0_u64);
        let (model_preference_tx, model_preference_rx) =
            watch::channel(config.model_preference.clone());
        let (result_tx, result_rx) = watch::channel(None);
        let compaction_busy = Arc::new(AtomicBool::new(false));
        let mailbox = Arc::new(RunMailbox::new());
        let channels = LoopChannels {
            phase_tx: phase_tx.clone(),
            message_count_tx,
            inbox_rx,
            cancel_rx,
            mailbox_version_rx: mailbox.subscribe_version(),
            compact_rx,
            model_preference_rx,
            compaction_busy: Arc::clone(&compaction_busy),
            result_tx,
        };
        let trigger = &restored.trigger;
        if trigger.kind != AgentMessageKind::Reply {
            self.shared
                .sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    trigger.message_id.clone(),
                    SentRecord {
                        sender,
                        recipient: run_id,
                    },
                );
        }
        let restored_event = LifecycleEvent::AgentRunRestored {
            run_id: run_id.to_string(),
            restored_by: trigger.sender_run_id.clone(),
            message_id: trigger.message_id.clone(),
        };
        let task = RunTask {
            run_id,
            role,
            prompt: String::new(),
            config: config.clone(),
            parent,
            mailbox: Arc::clone(&mailbox),
            handoff: None,
            restored: Some(restored),
        };
        runs.insert(
            run_id,
            RunEntry {
                role,
                name,
                model,
                config,
                parent,
                escalated_from: None,
                phase_tx,
                phase_rx,
                message_count_rx,
                inbox_tx,
                cancel_tx,
                compact_tx,
                model_preference_tx,
                result_rx,
                compaction_busy,
                mailbox,
                _join: None,
            },
        );
        self.shared.bus.emit(Event::new(restored_event));
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: run_id.to_string(),
                from: AgentRunPhase::Pending,
                to: AgentRunPhase::Pending,
                reason: Some("registered".into()),
            }));
        let join = tokio::spawn(run_agent(Arc::downgrade(&self.shared), task, channels));
        if let Some(entry) = runs.get_mut(&run_id) {
            entry._join = Some(join);
        }
    }
}
