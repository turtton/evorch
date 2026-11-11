//! Automatic question delivery never turns a skipped/Stop wake into acceptance.
mod common;

use runtime::{Role, RunConfig, StopScope};
use workbench_host::{
    CommandHost,
    commands::{CommandService, ContinueInput, HostCommand, HostEvent, NoticeOutcome},
};

#[test]
fn automatic_question_wake_keeps_stop_bookkeeping_until_explicit_continue() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("question-wake.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let model = std::sync::Arc::new(common::HeldModel::default());
    let bus = std::sync::Arc::new(event_bus::EventBus::new(128));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        std::sync::Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let (parent, child, question) = rt.block_on(async {
        let parent =
            runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
        model.reached.notified().await;
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        model.reached.notified().await;
        runtime.stop(parent, StopScope::SelfOnly).unwrap();
        runtime.wait(parent).await.unwrap();
        let question = runtime
            .request_user_question(child, "Which bounded scope?".into(), vec![], true)
            .unwrap();
        (parent, child, question)
    });
    let mut host = CommandHost::new(CommandService::new(runtime.clone(), rt.handle().clone()));
    host.service_mut()
        .bindings
        .chat_runs
        .insert("thread".into(), parent);
    host.service_mut()
        .bindings
        .stop_marked
        .insert("thread".into());
    host.service_mut()
        .bindings
        .stopped_by_us
        .insert("thread".into());
    let resume = |automatic| {
        HostCommand::ContinueChat(ContinueInput {
            project_root: None,
            thread_id: "thread".into(),
            composer_role: Default::default(),
            model_preference: None,
            automatic,
        })
    };
    let events = host.submit_local(resume(true), &workbench_host::provenance::local_desktop());
    assert!(matches!(
        events.as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    assert!(host.service().bindings.stop_marked.contains("thread"));
    assert!(host.service().bindings.stopped_by_us.contains("thread"));
    assert_eq!(
        runtime.inspect_agent(parent).unwrap().phase,
        event_bus::AgentRunPhase::Stopped
    );
    assert!(
        runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    let events = host.submit_local(resume(false), &workbench_host::provenance::local_desktop());
    assert!(
        matches!(events.as_slice(), [HostEvent::ChatAccepted { run_id, .. }] if run_id == &parent.to_string())
    );
    assert!(!host.service().bindings.stop_marked.contains("thread"));
    assert!(!host.service().bindings.stopped_by_us.contains("thread"));
    rt.block_on(async {
        runtime.cancel_subtree(parent).unwrap();
        runtime.wait(child).await.unwrap();
        runtime.wait(parent).await.unwrap();
    });
}
