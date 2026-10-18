use super::*;

async fn wait_and_stop(runtime: &AgentRuntime, run: RunId) {
    let mut phase = runtime.entry(run).unwrap().phase_rx.clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while *phase.borrow_and_update() != AgentRunPhase::Waiting {
            phase.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn legacy_chat_continues_saves_restarts_and_continues_with_thread_binding() {
    for legacy in ["Oracle", "oracle"] {
        // Include already-mixed snapshots written before this fix.
        for saved_role in [legacy, "Advisor"] {
            let fixture = Fixture::new();
            let config = storage::StorageConfig {
                db_path: fixture._dir.path().join("goal.sqlite3"),
                ..Default::default()
            };
            let run = fixture.runtime.delegate_background(
                Role::Advisor,
                "original advice request".into(),
                RunConfig {
                    name: Some(format!("chat:{legacy}:thread:consult")),
                    ..Default::default()
                },
            );
            fixture.runtime.wait(run).await.unwrap();
            let mut record = fixture
                .runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(run)
                .unwrap()
                .unwrap();
            let mut descriptor: RunRestoreDescriptor =
                serde_json::from_str(&record.config_json).unwrap();
            descriptor.role = saved_role.into();
            record.role = saved_role.into();
            record.config_json = serde_json::to_string(&descriptor).unwrap();
            fixture
                .storage
                .handle()
                .upsert_run_context(&record)
                .unwrap();
            drop(fixture.runtime);

            // Both passes use a fresh runtime, so no in-memory thread binding survives.
            for pass in 0..2 {
                let bus = Arc::new(EventBus::new(128));
                let runtime = AgentRuntime::new(
                    bus.clone(),
                    Arc::new(ToolExecutor::new(bus)),
                    Arc::new(CompletingModel),
                )
                .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap());
                runtime
                    .continue_goal(run, format!("followup {pass}"), RunConfig::default())
                    .unwrap();
                assert_eq!(runtime.goal_thread(run).as_deref(), Some("thread:consult"));
                assert_eq!(
                    runtime.entry(run).unwrap().config.name.as_deref(),
                    Some("chat:Advisor:thread:consult")
                );
                wait_and_stop(&runtime, run).await;
                record = runtime
                    .shared
                    .run_store
                    .get()
                    .unwrap()
                    .restore_record(run)
                    .unwrap()
                    .unwrap();
                descriptor = serde_json::from_str(&record.config_json).unwrap();
                assert_eq!(record.role, "Advisor");
                assert_eq!(descriptor.role, "Advisor");
                assert_eq!(
                    descriptor.name.as_deref(),
                    Some("chat:Advisor:thread:consult")
                );
                let messages: Vec<providers::Message> =
                    serde_json::from_str(&record.messages_json).unwrap();
                let text = serde_json::to_string(&messages).unwrap();
                assert!(text.contains("original advice request"));
                for prior in 0..=pass {
                    assert!(text.contains(&format!("followup {prior}")));
                }
            }
        }
    }
}

#[tokio::test]
async fn continuing_a_named_goal_does_not_turn_it_into_a_chat_name() {
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    fixture.runtime.bind_thread_root("thread", run).unwrap();
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    assert_eq!(
        fixture.runtime.entry(run).unwrap().config.name.as_deref(),
        Some("goal-1")
    );
    wait_and_stop(&fixture.runtime, run).await;
}
