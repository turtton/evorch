//! Build only the context-preparation state, never setup/execute/finalize an agent.
use super::*;

impl LoopState {
    pub(crate) async fn for_saved_compaction(
        runtime: &crate::AgentRuntime,
        run_id: RunId,
        role: Role,
        config: RunConfig,
        restored: crate::restore::RestoredState,
    ) -> Self {
        let weak = Arc::downgrade(&runtime.shared);
        let project = runtime.shared.run_project(&config);
        let mut shared = loop_shared(&weak, project.as_deref()).expect("runtime held");
        if let Some(resolution) = runtime.shared.model_resolution.get() {
            resolution.apply(&mut shared.compaction).await;
        }
        let image_model = shared
            .model
            .invocation_snapshot()
            .unwrap_or_else(|| shared.model.clone());
        // No executor setup, skill loading, inbox adoption, or agent completion.
        let policy = runtime.execution_policy(role).for_run_config(&config, true);
        let turn = restored
            .turn
            .clone()
            .unwrap_or_else(|| providers::RequestTurnContext::unattributed(false));
        let task = RunTask {
            publication: crate::runtime::ExecutionControl::new(false).publication(),
            run_id,
            role,
            prompt: String::new(),
            start: RunStart::Resume,
            config: config.clone(),
            parent: None,
            ancestry: Default::default(),
            mailbox: Arc::new(RunMailbox::new()),
            handoff: None,
            restored: None,
            turn,
            initial_prompt_id: None,
        };
        let channels = LoopChannels {
            phase_tx: watch::channel(AgentRunPhase::Done).0,
            message_count_tx: watch::channel(0).0,
            inbox_rx: tokio::sync::mpsc::channel(1).1.into(),
            user_inbox: Arc::new(crate::runtime::user_inbox::UserInbox::default()),
            cancel_rx: watch::channel(RunInterrupt::None).1,
            mailbox_version_rx: watch::channel(0).1,
            compact_rx: watch::channel(0).1,
            model_preference_rx: watch::channel(config.model_preference.clone()).1,
            compaction_busy: Arc::new(AtomicBool::new(false)),
            result_tx: watch::channel(None).0,
        };
        let mut context = AgentContext::from_restored(
            run_id,
            role,
            restored.messages,
            restored.checkpoints,
            restored.responses_replay,
        );
        context.provenance = restored.provenance;
        let mut state = Self::new(task, shared, channels, context, image_model, policy);
        state.continuity = restored.continuity;
        state.told_directories = *restored.allowed_directories;
        state.allowed_instructions = *restored.allowed_instructions;
        state
    }
}
