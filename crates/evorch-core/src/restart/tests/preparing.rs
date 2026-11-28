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
        attachments: vec![],
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

// The blocking DB-preflight worker is held, not the coordinator. No timing
// assumption: its entry and return are acknowledged by explicit channels.
#[tokio::test]
async fn pending_db_preflight_allows_stop_cancel_shutdown_and_new_generation() {
    for action in 0..4 {
        let (_dir, mut host) = boot().await;
        let core = RestartCoordinator::new(&host);
        let events = host.runtime.event_bus().subscribe();
        let run = host
            .runtime
            .delegate_chat(
                "preflight",
                runtime::Role::Orchestrator,
                "stop".into(),
                runtime::RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        waiting(run, events).await;
        let (entered, mut entry) = tokio::sync::mpsc::unbounded_channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Arc::new(Mutex::new(blocked));
        let (returned, mut returns) = tokio::sync::mpsc::unbounded_channel();
        *core.0.preflight_hook.lock().unwrap() = Some(Arc::new(move |id| {
            entered.send(id).unwrap();
            blocked.lock().unwrap().recv().unwrap();
            returned.send(()).unwrap();
        }));
        let id = core
            .request_planned(std::collections::BTreeMap::from([(
                run,
                runtime::quiesce::RecoveryRecipe {
                    settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: runtime::quiesce::ResumePolicy::CoreOwned,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                },
            )]))
            .unwrap();
        assert_eq!(entry.recv().await, Some(id));
        assert!(core.0.state.lock().unwrap().attempt.is_none());
        match action {
            0 => {
                host.runtime.stop(run, StopScope::SelfOnly).unwrap();
            }
            1 | 3 => {
                core.cancel(id).unwrap();
            }
            2 => core.transition_to_shutdown(),
            _ => unreachable!(),
        }
        assert_eq!(
            core.snapshot().phase,
            if action == 2 {
                RestartPhase::ShuttingDown
            } else {
                RestartPhase::Running
            }
        );
        assert!(core.0.state.lock().unwrap().attempt.is_none());
        *core.0.preflight_hook.lock().unwrap() = None;
        let next = if action == 3 {
            Some(core.request().unwrap())
        } else {
            None
        };
        release.send(()).unwrap();
        returns.recv().await.unwrap();
        if let Some(next) = next {
            core.wait_sealed(next).await.unwrap();
            assert_ne!(id, next);
            core.cancel(next).unwrap();
        }
        assert_ne!(core.snapshot().phase, RestartPhase::FailedClosed);
        host.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_cancels_while_db_preflight_is_pending_without_starting_an_attempt() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let (entered, pending) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let barrier = Mutex::new(Some((entered, blocked)));
    let (returned, complete) = tokio::sync::oneshot::channel();
    let returned = Mutex::new(Some(returned));
    *core.0.preflight_hook.lock().unwrap() = Some(Arc::new(move |_| {
        let (entered, blocked) = barrier.lock().unwrap().take().unwrap();
        entered.send(()).unwrap();
        blocked.recv().unwrap();
        returned.lock().unwrap().take().unwrap().send(()).unwrap();
    }));
    let id = core
        .request_planned(std::collections::BTreeMap::new())
        .unwrap();
    pending.await.unwrap();
    tokio::time::advance(PREPARATION_LIMIT).await;
    phase(&core, RestartPhase::Running).await;
    assert!(core.wait_sealed(id).await.is_err());
    assert!(core.0.state.lock().unwrap().attempt.is_none());
    release.send(()).unwrap();
    complete.await.unwrap();
    assert!(host.runtime.learning_gate().is_open());
    host.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
mod observation_retry {
    use super::*;
    use crate::restart::public::{ObservedRecipes, RecipeSource};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workbench_host::{admin::AdminIo, lane::BindingTable};

    struct Source {
        table: BindingTable,
        recipes: std::collections::BTreeMap<runtime::RunId, runtime::quiesce::RecoveryRecipe>,
        collected: AtomicUsize,
    }
    impl RecipeSource for Source {
        fn collect(&self) -> AdminIo<'_, ObservedRecipes> {
            Box::pin(async move {
                self.collected.fetch_add(1, Ordering::SeqCst);
                Ok(ObservedRecipes {
                    recipes: self.recipes.clone(),
                    bindings: self.table.observe(),
                })
            })
        }
    }

    async fn waiting_run(
        host: &BootstrappedHost,
    ) -> (
        runtime::RunId,
        std::collections::BTreeMap<runtime::RunId, runtime::quiesce::RecoveryRecipe>,
    ) {
        let events = host.runtime.event_bus().subscribe();
        let run = host
            .runtime
            .delegate_chat(
                "observed",
                runtime::Role::Orchestrator,
                "observe".into(),
                runtime::RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        waiting(run, events).await;
        let recipes = std::collections::BTreeMap::from([(
            run,
            runtime::quiesce::RecoveryRecipe {
                settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: runtime::quiesce::ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        )]);
        (run, recipes)
    }

    /// The recipe observation is taken again each time something changed under
    /// it (here: the binding generation moves while the DB preflight runs), and
    /// the restart then goes on from a clean pass.
    #[tokio::test]
    async fn a_change_during_observation_is_observed_again_and_the_restart_goes_on() {
        let (_dir, mut host) = boot().await;
        let core = RestartCoordinator::new(&host);
        let (_run, recipes) = waiting_run(&host).await;
        let table = BindingTable::default();
        let source = Arc::new(Source {
            table: table.clone(),
            recipes: recipes.clone(),
            collected: AtomicUsize::new(0),
        });
        let disturbed = Arc::new(AtomicUsize::new(0));
        *core.0.preflight_hook.lock().unwrap() = Some(Arc::new({
            let disturbed = disturbed.clone();
            move |_| {
                if disturbed.fetch_add(1, Ordering::SeqCst) < 2 {
                    table.replace(Default::default());
                }
            }
        }));
        let id = core
            .request_with_source(Some(recipes), PREPARATION_LIMIT, Some(source.clone()))
            .unwrap();
        core.wait_sealed(id).await.unwrap();
        assert_eq!(
            source.collected.load(Ordering::SeqCst),
            3,
            "two disturbed passes, then a clean one"
        );
        assert!(core.0.state.lock().unwrap().attempt.is_some());
        core.cancel(id).unwrap();
        host.shutdown().await.unwrap();
    }

    /// A run set that never settles does not keep the request alive: after the
    /// bounded number of observations it is cancelled, with no attempt begun.
    #[tokio::test]
    async fn an_observation_that_never_settles_cancels_after_the_bound() {
        let (_dir, mut host) = boot().await;
        let core = RestartCoordinator::new(&host);
        let (_run, recipes) = waiting_run(&host).await;
        let table = BindingTable::default();
        let source = Arc::new(Source {
            table: table.clone(),
            recipes: recipes.clone(),
            collected: AtomicUsize::new(0),
        });
        *core.0.preflight_hook.lock().unwrap() = Some(Arc::new(move |_| {
            table.replace(Default::default());
        }));
        let id = core
            .request_with_source(Some(recipes), PREPARATION_LIMIT, Some(source.clone()))
            .unwrap();
        phase(&core, RestartPhase::Running).await;
        assert!(core.wait_sealed(id).await.is_err());
        assert_eq!(
            source.collected.load(Ordering::SeqCst),
            crate::restart::preparing::OBSERVATION_ATTEMPTS
        );
        assert!(core.0.state.lock().unwrap().attempt.is_none());
        assert_eq!(host.runtime.quiesce_state(), QuiesceState::Running);
        host.shutdown().await.unwrap();
    }
}
