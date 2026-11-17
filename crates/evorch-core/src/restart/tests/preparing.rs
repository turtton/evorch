use super::*;
use runtime::quiesce::QuiesceState;

fn send_chat(host: &BootstrappedHost) -> runtime::RunId {
    use workbench_protocol::{commands::*, ids::*, namespaces::ConversationCommand};
    let message = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("preparing-chat").unwrap(),
        submission_id: SubmissionId::new("during-drain").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "accepted during evaluation drain".into(),
        skills: vec![],
        images: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let mut host = host.host.lock().unwrap();
    host.reserve_new_chat_thread(&message).unwrap();
    host.handle_command(
        Command::Conversation(ConversationCommand::SendChat(message)),
        workbench_host::provenance::local_desktop(),
    )
    .unwrap()
    .run_id
    .unwrap()
    .as_str()
    .parse()
    .unwrap()
}

#[tokio::test]
async fn evaluation_drain_accepts_and_delivers_chat_before_runtime_attempt() {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let evaluations = host.restart_preparing_services().evaluations;
    let (release, returned) = tokio::sync::oneshot::channel();
    evaluations
        .spawn(async { returned.await.unwrap() })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::PreparingDrains);
    assert_eq!(core.request().unwrap(), id);
    assert!(evaluations.producers.try_enter().is_none());
    assert!(host.runtime.learning_gate().try_enter().is_none());
    assert!(host.runtime.producer_gate().try_enter().is_some());
    assert_eq!(host.runtime.quiesce_state(), QuiesceState::Running);
    let events = host.runtime.event_bus().subscribe();
    let run = send_chat(&host);
    waiting(run, events).await;
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(core.snapshot().phase, RestartPhase::PreparingDrains);
    release.send(()).unwrap();
    core.wait_sealed(id).await.unwrap();
    assert!(!evaluations.is_busy());
    core.cancel(id).unwrap();
    assert!(evaluations.producers.is_open());
    assert!(host.runtime.learning_gate().is_open());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_before_suspend_does_not_replay_or_stop_evaluation() {
    let (_dir, mut host) = boot().await;
    let evaluations = host.restart_preparing_services().evaluations;
    let (release, returned) = tokio::sync::oneshot::channel();
    evaluations
        .spawn(async { returned.await.unwrap() })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let runtime_changes = host.runtime.subscribe_restart_state();
    let id = core.request().unwrap();
    core.cancel(id).unwrap();
    assert!(!runtime_changes.has_changed().unwrap());
    assert_eq!(core.snapshot().phase, RestartPhase::Running);
    assert!(evaluations.is_busy());
    assert!(evaluations.producers.is_open());
    assert!(host.runtime.learning_gate().is_open());
    let next = core.request().unwrap();
    assert_ne!(next, id);
    assert!(core.cancel(id).is_err());
    assert!(!evaluations.producers.is_open());
    release.send(()).unwrap();
    core.wait_sealed(next).await.unwrap();
    core.cancel(next).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_and_preparing_both_orders_preserve_new_generation_pause() {
    for (direct, stop_first) in [(false, true), (false, false), (true, true), (true, false)] {
        let (_dir, mut host) = boot().await;
        let mut events = host.runtime.event_bus().subscribe();
        let run = host.runtime.delegate_background(
            runtime::Role::Worker,
            "idle stop target".into(),
            runtime::RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        loop {
            if matches!(events.recv().await.unwrap().kind, event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Waiting, .. }
            ) if run_id == run.to_string())
            {
                break;
            }
        }
        let evaluations = host.restart_preparing_services().evaluations;
        let (release, returned) = tokio::sync::oneshot::channel();
        evaluations
            .spawn(async { returned.await.unwrap() })
            .unwrap();
        let core = RestartCoordinator::new(&host);
        let stop = || {
            if direct {
                host.runtime.stop(run, StopScope::SelfOnly)
            } else {
                core.stop(run, StopScope::SelfOnly)
            }
        };
        if stop_first {
            stop().unwrap();
        }
        let id = core.request().unwrap();
        if stop_first {
            assert_eq!(core.snapshot().phase, RestartPhase::PreparingDrains);
            core.cancel(id).unwrap();
        } else {
            stop().unwrap();
        }
        assert_eq!(core.snapshot().phase, RestartPhase::Running);
        assert!(core.0.state.lock().unwrap().attempt.is_none());
        assert_eq!(host.runtime.quiesce_state(), QuiesceState::Running);
        assert!(evaluations.is_busy());
        assert!(evaluations.producers.is_open());
        assert!(host.runtime.learning_gate().is_open());
        assert!(core.cancel(id).is_err());
        let next = core.request().unwrap();
        assert_ne!(next, id);
        assert!(!evaluations.producers.is_open());
        assert!(!host.runtime.learning_gate().is_open());
        release.send(()).unwrap();
        core.wait_sealed(next).await.unwrap();
        assert!(!evaluations.producers.is_open());
        assert!(!host.runtime.learning_gate().is_open());
        core.cancel(next).unwrap();
        host.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn preparing_deadline_is_not_renewed_by_joined_requests() {
    let (_dir, mut host) = boot().await;
    let evaluations = host.restart_preparing_services().evaluations;
    let (release, returned) = tokio::sync::oneshot::channel();
    evaluations
        .spawn(async { returned.await.unwrap() })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    tokio::time::advance(PREPARATION_LIMIT - std::time::Duration::from_secs(1)).await;
    assert_eq!(core.request().unwrap(), id);
    assert_eq!(core.snapshot().phase, RestartPhase::PreparingDrains);
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    phase(&core, RestartPhase::Running).await;
    assert_eq!(host.runtime.quiesce_state(), QuiesceState::Running);
    assert!(evaluations.is_busy());
    assert!(evaluations.producers.is_open());
    release.send(()).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn evaluation_join_failure_closes_restart_without_starting_an_attempt() {
    let (_dir, mut host) = boot().await;
    host.restart_preparing_services()
        .evaluations
        .spawn(async { panic!("evaluation fault") })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    assert!(core.wait_sealed(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert!(core.0.state.lock().unwrap().attempt.is_none());
    assert!(core.cancel(id).is_err());
    assert!(!host.runtime.producer_gate().is_open());
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn health_is_rechecked_after_natural_drain_before_attempt_creation() {
    let (_dir, mut host) = boot().await;
    let (release, returned) = tokio::sync::oneshot::channel();
    host.restart_preparing_services()
        .evaluations
        .spawn(async { returned.await.unwrap() })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "health failure".into(),
        Default::default(),
    );
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_preparing_terminal BEFORE UPDATE ON resume_intents BEGIN SELECT RAISE(ABORT, 'terminal failure'); END; CREATE TRIGGER reject_preparing_insert BEFORE INSERT ON resume_intents BEGIN SELECT RAISE(ABORT, 'terminal failure'); END;").unwrap();
    host.runtime.wait(run).await.unwrap();
    assert!(matches!(
        host.runtime.quiesce_state(),
        QuiesceState::Failed { .. }
    ));
    release.send(()).unwrap();
    assert!(core.wait_sealed(id).await.is_err());
    assert!(core.0.state.lock().unwrap().attempt.is_none());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    db.execute_batch(
        "DROP TRIGGER reject_preparing_terminal; DROP TRIGGER reject_preparing_insert",
    )
    .unwrap();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn rejected_direct_stop_during_drains_does_not_release_preparation() {
    let (_dir, mut host) = boot().await;
    let evaluations = host.restart_preparing_services().evaluations;
    let (release, returned) = tokio::sync::oneshot::channel();
    evaluations
        .spawn(async { returned.await.unwrap() })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    assert!(matches!(
        host.runtime.stop(RunId::new(u64::MAX), StopScope::SelfOnly),
        Err(RuntimeError::UnknownRun { .. })
    ));
    assert_eq!(core.snapshot().phase, RestartPhase::PreparingDrains);
    assert_eq!(core.snapshot().id, Some(id));
    assert!(!evaluations.producers.is_open());
    assert!(!host.runtime.learning_gate().is_open());
    assert!(host.runtime.producer_gate().is_open());
    release.send(()).unwrap();
    core.wait_sealed(id).await.unwrap();
    core.cancel(id).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_boundary_does_not_keep_a_released_lifecycle_owner_alive() {
    let (_dir, mut host) = boot().await;
    // Deliberately do not install a strong host-side handler in this fixture.
    let core = RestartCoordinator::from_parts(
        host.runtime.clone(),
        HostRestartGate::default(),
        host.restart_producers(),
        host.restart_preparing_services(),
    );
    let weak = Arc::downgrade(&core.0);
    drop(core);
    assert!(
        weak.upgrade().is_none(),
        "runtime must not own the coordinator"
    );
    assert!(matches!(
        host.runtime.stop(RunId::new(u64::MAX), StopScope::SelfOnly),
        Err(RuntimeError::RestartBusy)
    ));
    host.shutdown().await.unwrap();
}
