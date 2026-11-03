//! ADR 0044 §2/§17: a saved child Orchestrator is refused at restore with its
//! original snapshot kept; a root Orchestrator restores as before.

use super::*;
use event_bus::AgentMessageKind;
use runtime::{RunRestoreFailure, RuntimeError};

fn model() -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(
        (0..16).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ))
}

#[tokio::test]
async fn saved_child_orchestrator_is_refused_and_its_snapshot_kept() {
    // Given: a finished child saved as a nested Orchestrator, as an older version
    // that allowed the delegation would have left it.
    let (_dir, config, storage, database) = storage_fixture();
    let (first, _) = runtime_with(model());
    let first = first.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        first.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&first, parent).await;
    let child = first
        .delegate_background_as_child(parent, Role::Worker, "legacy", RunConfig::default())
        .unwrap();
    terminal(&first, child).await;
    let record = database.run_context(&child.to_string()).unwrap().unwrap();
    let mut descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
    descriptor["role"] = serde_json::json!("Orchestrator");
    drop(first);
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute(
        "UPDATE run_contexts SET role = 'Orchestrator', config_json = ?2 WHERE run_id = ?1",
        rusqlite::params![child.to_string(), descriptor.to_string()],
    )
    .unwrap();
    let saved = database.run_context(&child.to_string()).unwrap().unwrap();
    let (runtime, bus) = runtime_with(model());
    let mut events = bus.subscribe();
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    runtime.spawn_reserved(
        parent,
        None,
        Role::Orchestrator,
        "parent",
        RunConfig::default(),
    );
    terminal(&runtime, parent).await;

    // When: the live root parent messages the saved child.
    let error = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "revive", None)
        .expect_err("a nested Orchestrator is never revived");

    // Then: the refusal names the parent, and nothing was registered or rewritten.
    let RuntimeError::RunRestoreFailed {
        run_id,
        reason: RunRestoreFailure::NestedOrchestrator { parent: named, .. },
    } = error
    else {
        panic!("expected the nested-Orchestrator refusal: {error:?}");
    };
    assert_eq!(run_id, child.to_string());
    assert_eq!(named, parent.to_string());
    assert!(runtime.list_agents().iter().all(|run| run.run_id != child));
    let after = database.run_context(&child.to_string()).unwrap().unwrap();
    assert_eq!(after, saved, "the original snapshot is evidence");

    // And: the refusal is also recorded as a diagnostic naming the run.
    let diagnostic = loop {
        if let EventKind::Diagnostic(diagnostic) = events.recv().await.unwrap().kind
            && diagnostic.code == "NestedOrchestratorRejected"
        {
            break diagnostic;
        }
    };
    assert_eq!(diagnostic.run_id, Some(child.to_string()));
    assert!(diagnostic.detail.contains(&parent.to_string()));
}

async fn through_waiting(events: &mut event_bus::EventReceiver, run: RunId) {
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Waiting,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            return;
        }
    }
}

#[tokio::test]
async fn saved_root_orchestrator_chat_still_resumes() {
    // Given: a root Orchestrator conversation stopped before a restart.
    let (_dir, config, storage, _) = storage_fixture();
    let (first, bus) = runtime_with(model());
    let mut events = bus.subscribe();
    let first = first.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let root = first
        .delegate_chat(
            "root",
            Role::Orchestrator,
            "Task".into(),
            RunConfig {
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    through_waiting(&mut events, root).await;
    first.stop(root, runtime::StopScope::SelfOnly).unwrap();
    terminal(&first, root).await;
    drop(first);
    let (runtime, bus) = runtime_with(model());
    let mut events = bus.subscribe();
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());

    // When: the new process resumes it.
    let resumed = runtime.resume_chat(root, RunConfig::default());

    // Then: a root Orchestrator is not mistaken for a nested one.
    assert_eq!(resumed, Ok(root));
    through_waiting(&mut events, root).await;
    runtime.cancel(root).unwrap();
    terminal(&runtime, root).await;
}
