use super::*;

// Current-thread execution deliberately does not yield between observation,
// registration and reset. The spawned model loop cannot mask a missing hook by
// registering a scratch Run user first.
#[tokio::test]
async fn restored_publication_invalidates_unused_scratch_observation() {
    for (configured, child) in [(true, false), (true, true), (false, false), (false, true)] {
        let bus = Arc::new(EventBus::new(64));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
        );
        let root = runtime.reserve_run_id();
        runtime.bind_thread_root("thread", root).unwrap();
        let run = if child {
            runtime.reserve_run_id()
        } else {
            root
        };
        let parent = child.then_some(root);
        let observation = configured
            .then(|| crate::scratch::publication_tests::Observation::new(&runtime, "thread"));
        let scratch_key = runtime.restored_scratch_publication_key(run, parent);
        let trigger = AgentMessage {
            message_id: "restore".into(),
            sender_run_id: "runtime".into(),
            recipient_run_id: run.to_string(),
            kind: AgentMessageKind::Send,
            content: String::new(),
            reply_to: None,
        };
        {
            let mut intents = runtime.shared.spawn_intents.lock().unwrap();
            let mut admissions = runtime.shared.admissions.lock().unwrap();
            let execution =
                AgentRuntime::prepare_restored_execution(run, parent, &intents).unwrap();
            AgentRuntime::install_restored_execution(
                run,
                parent,
                &mut intents,
                &mut admissions,
                &execution,
            );
            runtime
                .register_restored(
                    &mut lock_runs(&runtime.shared.runs),
                    scratch_key,
                    &intents,
                    execution,
                    (run, parent, Role::Worker, RunConfig::default(), run),
                    RestoredState {
                        input_selection_transition: None,
                        recovery_waits: Vec::new(),
                        tool_descriptions: None,
                        mcp_snapshot: None,
                        harness_notice_pending: false,
                        provenance: Default::default(),
                        snapshot_revision: crate::restore::revision::SnapshotRevision::fresh(),
                        responses_replay: Default::default(),
                        host_inputs: Vec::new(),
                        admission_reservation: None,
                        turn: None,
                        boundary_pending: false,
                        continuity: Default::default(),
                        pending_reference_id: None,
                        messages: vec![],
                        checkpoints: vec![],
                        trigger: None,
                        turn_completed: true,
                        allowed_directories: Default::default(),
                        allowed_instructions: Default::default(),
                        project_rule_injections: Default::default(),
                        plan_notice: Default::default(),
                        loaded_skills: Vec::new(),
                    },
                    &trigger,
                    None,
                    false,
                )
                .unwrap();
        }
        if let Some(observation) = observation {
            observation.assert_preserved();
        } else {
            assert!(runtime.shared.thread_scratch.lock().unwrap().is_none());
        }
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}
