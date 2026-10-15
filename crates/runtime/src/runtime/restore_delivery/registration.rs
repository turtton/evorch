use super::*;

impl AgentRuntime {
    pub(in crate::runtime) fn register_restored(
        &self,
        runs: &mut HashMap<RunId, RunEntry>,
        identity: (RunId, Option<RunId>, Role, RunConfig, RunId),
        restored: RestoredState,
        trigger: &AgentMessage,
    ) {
        let (run_id, parent, role, mut config, sender) = identity;
        self.install_run_ownership_guard(run_id, &mut config);
        // Durable question answers are flushed by the loop. They need neither
        // a synthetic message nor a rewrite of the saved provider prefix.
        let history_only = restored.trigger.is_none();
        let name = runs.get(&run_id).map_or_else(
            || config.name.clone().unwrap_or_else(|| role.name().into()),
            |entry| entry.name.clone(),
        );
        let model = runs.get(&run_id).map_or_else(
            || {
                self.shared
                    .model
                    .selected_model(role, config.category.as_deref())
            },
            |entry| entry.model.clone(),
        );
        let (phase_tx, phase_rx) = watch::channel(AgentRunPhase::Pending);
        let (message_count_tx, message_count_rx) =
            watch::channel(restored.messages.len() + usize::from(!history_only));
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let user_inbox = Arc::new(UserInbox::default());
        let (cancel_tx, cancel_rx) = watch::channel(RunInterrupt::None);
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
            user_inbox: Arc::clone(&user_inbox),
            cancel_rx,
            mailbox_version_rx: mailbox.subscribe_version(),
            compact_rx,
            model_preference_rx,
            compaction_busy: Arc::clone(&compaction_busy),
            result_tx,
        };
        if !history_only && trigger.kind != AgentMessageKind::Reply {
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
        let task = restored.attach_to(RunTask {
            run_id,
            role,
            prompt: String::new(),
            start: if history_only {
                crate::agent_loop::RunStart::Resume
            } else {
                crate::agent_loop::RunStart::WithInput
            },
            config: config.clone(),
            parent,
            mailbox: Arc::clone(&mailbox),
            handoff: None,
            restored: None,
        });
        let completion_relayed = !history_only
            && runs
                .get(&run_id)
                .is_none_or(|entry| entry.completion_relayed);
        runs.insert(
            run_id,
            RunEntry {
                terminal_reason: None,
                role,
                name,
                model,
                config,
                parent,
                escalated_from: None,
                completion_relayed,
                phase_tx,
                phase_rx,
                message_count_rx,
                inbox_tx,
                user_inbox,
                pending_reference: Arc::new(Mutex::new(None)),
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

#[cfg(test)]
mod review_tests {
    use super::*;

    #[tokio::test]
    async fn restored_registration_defers_review_until_trusted_input_materializes_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        for has_evidence in [false, true] {
            let run = runtime.reserve_run_id();
            if has_evidence {
                storage
                    .handle()
                    .record_review_evidence(&[crate::run_store::evidence_row(
                        &run.to_string(),
                        &run.to_string(),
                        "initial_prompt",
                        "first",
                    )])
                    .unwrap();
            }
            let trigger = AgentMessage {
                message_id: format!("trigger-{run}"),
                sender_run_id: run.to_string(),
                recipient_run_id: run.to_string(),
                kind: AgentMessageKind::Send,
                content: "internal".into(),
                reply_to: None,
            };
            runtime.register_restored(
                &mut lock_runs(&runtime.shared.runs),
                (run, None, Role::Worker, RunConfig::default(), run),
                RestoredState {
                    messages: vec![],
                    checkpoints: vec![],
                    trigger: None,
                    turn_completed: true,
                    allowed_directories: Default::default(),
                    allowed_instructions: Default::default(),
                },
                &trigger,
            );
            assert!(
                !runtime
                    .shared
                    .review_runs
                    .lock()
                    .unwrap()
                    .contains_key(&run)
            );
            let result = runtime.send_message(run, "followup".into());
            let rows = runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .review_evidence_for_lineage(&[run.to_string()])
                .unwrap();
            if has_evidence {
                result.unwrap();
                assert_eq!(rows.len(), 2);
                assert_eq!(
                    runtime.shared.review_runs.lock().unwrap()[&run]
                        .requests()
                        .0
                        .len(),
                    2
                );
                assert_eq!(runtime.follow_up_status(run).unwrap().pending, 1);
            } else {
                assert!(matches!(result, Err(RuntimeError::ReviewEvidence { .. })));
                assert!(rows.is_empty());
                assert_eq!(runtime.follow_up_status(run).unwrap().pending, 0);
            }
            runtime.cancel(run).unwrap();
        }
    }
}
