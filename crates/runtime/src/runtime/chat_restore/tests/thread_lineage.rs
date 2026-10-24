use super::*;
use storage::threads::ThreadLineageKind;

fn store(fixture: &Fixture) -> &crate::RunStore {
    fixture.runtime.shared.run_store.get().unwrap()
}

#[tokio::test]
async fn chat_fork_and_children_persist_their_own_thread_identity() {
    let fixture = Fixture::new();
    let config = RunConfig {
        project_root: Some(fixture._dir.path().to_path_buf()),
        ..RunConfig::default()
    };
    let source = fixture
        .runtime
        .delegate_chat("source", Role::Worker, "hello".into(), config.clone())
        .unwrap();
    let row = store(&fixture).thread("source").unwrap().unwrap();
    assert_eq!(row.root_run_id, Some(source.to_string()));
    assert_eq!(row.chat_role.as_deref(), Some("Worker"));
    assert_eq!(
        row.project_root,
        Some(
            fixture
                ._dir
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    fixture.runtime.wait(source).await.unwrap();
    let record = store(&fixture).restore_record(source).unwrap().unwrap();
    let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    assert_eq!(descriptor.thread_id.as_deref(), Some("source"));
    let messages: Vec<providers::Message> = serde_json::from_str(&record.messages_json).unwrap();
    let len = messages
        .iter()
        .filter(|message| message.role != providers::Role::System)
        .count() as u64;
    let fork = fixture
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "branch".into(),
            config.clone(),
            Some(crate::ChatForkSeed {
                source_run_id: source.to_string(),
                context_len: len,
            }),
        )
        .unwrap();
    let row = store(&fixture).thread("fork").unwrap().unwrap();
    assert_eq!(row.parent_thread_id.as_deref(), Some("source"));
    assert_eq!(row.fork_point_run_id, Some(source.to_string()));
    assert_eq!(row.fork_point_context_len, Some(len));
    assert_eq!(row.lineage_kind, None);
    fixture.runtime.wait(fork).await.unwrap();
    let record = store(&fixture).restore_record(fork).unwrap().unwrap();
    let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    assert_eq!(
        descriptor.thread_id.as_deref(),
        Some("fork"),
        "source descriptor must not bind the fork to source"
    );
    let child = fixture
        .runtime
        .delegate_background_as_child(fork, Role::Worker, "child", config)
        .unwrap();
    fixture.runtime.wait(child).await.unwrap();
    assert_eq!(
        fixture.runtime.thread_of_live_run(child).as_deref(),
        Some("fork")
    );
    assert_eq!(
        store(&fixture)
            .thread_of_run(&child.to_string())
            .unwrap()
            .unwrap()
            .thread_id,
        "fork"
    );
    let record = store(&fixture).restore_record(child).unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<RunRestoreDescriptor>(&record.config_json)
            .unwrap()
            .thread_id
            .as_deref(),
        Some("fork")
    );

    // Exercise A-2b's narrow read facade against persisted roots and children.
    let ids = vec![fork.to_string(), child.to_string()];
    assert_eq!(store(&fixture).runs_of_thread("fork").unwrap().len(), 2);
    assert_eq!(store(&fixture).run_contexts_for(&ids).unwrap().len(), 2);
    assert_eq!(store(&fixture).terminal_summaries(&ids).unwrap().len(), 2);
    assert!(store(&fixture).latest_ledgers(&ids).unwrap().is_empty());
    assert!(
        store(&fixture)
            .user_questions_for_runs(&ids)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store(&fixture).thread_children("source").unwrap().len(), 1);
    assert_eq!(
        store(&fixture)
            .threads_in_project(row.project_root.as_deref().unwrap(), None, 20)
            .unwrap()
            .threads
            .len(),
        2
    );
}

#[tokio::test]
async fn escalation_records_parent_before_goal_binding_is_handed_off() {
    let fixture = Fixture::new();
    let config = RunConfig::default();
    let source = fixture
        .runtime
        .delegate_chat("source", Role::Worker, "hello".into(), config.clone())
        .unwrap();
    fixture.runtime.wait(source).await.unwrap();
    let escalated = fixture
        .runtime
        .spawn_escalated_root(
            crate::EscalationMemo {
                source_run_id: source,
                original_request: "hello".into(),
                findings: vec![],
                files_touched: vec![],
                blockers: vec![],
                workspace_state: String::new(),
                escalation_reason: "needs orchestration".into(),
                suggested_next: "continue".into(),
            },
            &config,
            Default::default(),
            || {},
        )
        .unwrap();
    let id = event_bus::escalation_thread_id(&escalated.to_string());
    let row = store(&fixture).thread(&id).unwrap().unwrap();
    assert_eq!(row.parent_thread_id.as_deref(), Some("source"));
    assert_eq!(row.escalation_source_run_id, Some(source.to_string()));
    assert_eq!(row.lineage_kind, Some(ThreadLineageKind::Escalation));
    assert_eq!(row.chat_role.as_deref(), Some("Orchestrator"));
    assert_eq!(fixture.runtime.goal_thread(source), None);
    fixture.runtime.wait(escalated).await.unwrap();
    assert_eq!(
        store(&fixture)
            .thread_of_run(&escalated.to_string())
            .unwrap()
            .unwrap()
            .thread_id,
        id
    );
    let record = store(&fixture).restore_record(escalated).unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<RunRestoreDescriptor>(&record.config_json)
            .unwrap()
            .thread_id,
        Some(id)
    );
}

#[tokio::test]
async fn live_thread_resolution_bounds_cycles_missing_parents_and_64_hops() {
    let fixture = Fixture::new();
    let root = fixture
        .runtime
        .delegate_chat("root", Role::Worker, "root".into(), RunConfig::default())
        .unwrap();
    fixture.runtime.wait(root).await.unwrap();
    let mut parent = root;
    for depth in 1..=65 {
        let child = fixture
            .runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        fixture.runtime.wait(child).await.unwrap();
        assert_eq!(
            fixture.runtime.thread_of_live_run(child).is_some(),
            depth <= 64
        );
        parent = child;
    }
    {
        let mut runs = lock_runs(&fixture.runtime.shared.runs);
        runs.get_mut(&root).unwrap().parent = Some(parent);
    }
    assert_eq!(fixture.runtime.thread_of_live_run(root), None);
    assert_eq!(
        fixture
            .runtime
            .thread_of_live_run("run-999999".parse().unwrap()),
        None
    );
}
