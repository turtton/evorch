use super::*;

struct MissingProvider;
#[async_trait::async_trait]
impl AgentModel for MissingProvider {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        Err(RuntimeError::Model {
            reason: "provider not registered".into(),
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "missing".into()
    }
}

#[tokio::test]
async fn immediate_provider_error_still_delivers_the_committed_initial_prompt() {
    let fixture = Fixture::with_model(Arc::new(MissingProvider));
    let run = fixture
        .runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "initial input".into(),
            RunConfig {
                initial_prompt_id: Some("initial".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Error
    );
    let receipt = fixture
        .runtime
        .host_input_receipts(&["initial".into()])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(receipt.status, storage::HostInputStatus::Delivered);
    let record = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    assert!(record.messages_json.contains("initial input"));
    assert_eq!(record.host_inputs_json, r#"["initial"]"#);
}

#[tokio::test]
async fn failure_before_spawn_drops_the_accepted_prompt() {
    let fixture = Fixture::new();
    // Empty thread IDs fail root binding after the accepted transaction.
    assert!(
        fixture
            .runtime
            .delegate_chat(
                "",
                Role::Worker,
                "never spawned".into(),
                RunConfig {
                    initial_prompt_id: Some("spawn-failed".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
    let receipt = fixture
        .runtime
        .host_input_receipts(&["spawn-failed".into()])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(receipt.status, storage::HostInputStatus::Dropped);
    assert_eq!(receipt.reason.as_deref(), Some("spawn_failed"));
    assert!(fixture.runtime.list_agents().is_empty());
}

#[tokio::test]
async fn same_run_restore_reconciles_old_receipts_before_inserting_the_new_input() {
    let fixture = Fixture::new();
    let run = fixture
        .runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "initial input".into(),
            RunConfig {
                initial_prompt_id: Some("initial".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    let handle = fixture.storage.handle();
    let old = storage::HostInputReceipt {
        prompt_id: "old".into(),
        run_id: run.to_string(),
        root_run_id: run.to_string(),
        text: "left behind".into(),
        image_count: 0,
        status: storage::HostInputStatus::Accepted,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
    };
    handle
        .append_host_input_accepted(&[], &[], None, &old)
        .unwrap();
    // Simulate a delivered receipt from an earlier incarnation whose snapshot was lost.
    let mut record = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    record.host_inputs_json = "[]".into();
    handle.upsert_run_context(&record).unwrap();
    fixture
        .runtime
        .continue_goal(
            run,
            "new input".into(),
            RunConfig {
                initial_prompt_id: Some("new".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let receipts = fixture
        .runtime
        .host_input_receipts(&["initial".into(), "old".into(), "new".into()])
        .unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt.status)
            .collect::<Vec<_>>(),
        [
            storage::HostInputStatus::Unknown,
            storage::HostInputStatus::Unknown,
            storage::HostInputStatus::Delivered
        ]
    );
    let record = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    assert_eq!(record.host_inputs_json, r#"["new"]"#);
    // Before the executor runs the resumed loop, the restored prompt is already durable once.
    assert_eq!(record.messages_json.matches("new input").count(), 1);
    fixture
        .runtime
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    fixture.runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn rejected_provider_admission_drops_a_fresh_input_but_keeps_a_restored_delivery() {
    let model = Arc::new(super::registration::AdmissionModel {
        reject: AtomicBool::new(true),
    });
    let fixture = Fixture::with_model(model.clone());
    let run = fixture
        .runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "not admitted".into(),
            RunConfig {
                initial_prompt_id: Some("not-admitted".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    let receipt = fixture
        .runtime
        .host_input_receipts(&["not-admitted".into()])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(receipt.status, storage::HostInputStatus::Dropped);
    model.reject.store(false, Ordering::Relaxed);
    let run = fixture
        .runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "admitted".into(),
            RunConfig {
                initial_prompt_id: Some("admitted".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    model.reject.store(true, Ordering::Relaxed);
    fixture
        .runtime
        .continue_goal(
            run,
            "saved before admission".into(),
            RunConfig {
                initial_prompt_id: Some("restored".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    assert_eq!(
        fixture
            .runtime
            .host_input_receipts(&["restored".into()])
            .unwrap()[0]
            .status,
        storage::HostInputStatus::Delivered
    );
}

#[tokio::test]
async fn invalid_team_setup_seals_and_settles_every_accepted_input_before_error() {
    let fixture = Fixture::new();
    let run = fixture
        .runtime
        .delegate_chat(
            "invalid-team",
            Role::Orchestrator,
            "not started".into(),
            RunConfig {
                topology: crate::CoordinationTopology::DynamicTeam { max_workers: 1 },
                initial_prompt_id: Some("setup-failed".into()),
                ..Default::default()
            },
        )
        .unwrap();
    // Current-thread execution has registered the inbox but not run the failing setup yet.
    fixture
        .runtime
        .send_message_with_images(
            run,
            "racing follow-up".into(),
            vec![],
            Some("setup-follow-up".into()),
        )
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Error
    );
    let receipts = fixture
        .runtime
        .host_input_receipts(&["setup-failed".into(), "setup-follow-up".into()])
        .unwrap();
    assert_eq!(receipts[0].status, storage::HostInputStatus::Dropped);
    assert_eq!(receipts[1].status, storage::HostInputStatus::Unknown);
    assert!(fixture.runtime.send_message(run, "late".into()).is_err());
}
