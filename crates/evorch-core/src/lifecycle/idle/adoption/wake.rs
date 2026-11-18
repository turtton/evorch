use super::clock::ManualClock;
use super::*;

fn interactive() -> runtime::RunConfig {
    runtime::RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    }
}
fn backend(host: &workbench_host::bootstrap::BootstrappedHost) -> Arc<crate::backend::HostBackend> {
    Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "wake-adoption-core".into(),
    ))
}
fn assert_adopting(host: &workbench_host::bootstrap::BootstrappedHost, run: runtime::RunId) {
    let info = host
        .runtime
        .list_agents()
        .into_iter()
        .find(|entry| entry.run_id == run)
        .unwrap();
    assert_eq!(info.phase, AgentRunPhase::Waiting);
    assert_eq!(host.runtime.follow_up_status(run).unwrap().pending, 0);
    assert!(host.runtime.is_adopting_user_input(run));
    assert!(!host.runtime.is_between_chat_turns(run));
    assert!(host.runtime.has_active_runs());
    assert!(host.busy());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drained_child_completion_retains_core_through_publication_with_zero_grace() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let model = Arc::new(ChatModel::default());
    let mut host = open_with_model(&dir, model.clone()).await;
    host.config.core.idle_grace_secs = 0;
    let mut events = host.runtime.event_bus().subscribe();
    let parent = host.runtime.delegate_background(
        runtime::Role::Orchestrator,
        "interactive parent".into(),
        interactive(),
    );
    waiting(&mut events, parent).await;
    let passive = hold_passive_wait(&host.runtime, parent).await;
    assert!(host.runtime.is_between_chat_turns(parent));
    assert!(!host.busy());
    let start = Instant::now();
    let clock = ManualClock::new(start);
    let grace = Duration::from_secs(60);
    let mut idle = IdleGrace::new(grace);
    assert_eq!(
        idle.update_at(start, false, 0, host.busy()),
        Some(start + grace)
    );
    let (reached, held) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    host.runtime
        .gate_next_input_wake_adoption(parent, reached, resumed)
        .unwrap();
    let child = host
        .runtime
        .delegate_background_as_child(
            parent,
            runtime::Role::Worker,
            "child completion wake",
            runtime::RunConfig::default(),
        )
        .unwrap();
    model.entered.notified().await;
    model.release.notify_one();
    assert_eq!(host.runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    passive.send(()).unwrap();
    held.await.unwrap();
    // The child is terminal and its real completion notice has already been
    // drained/injected. Only the adoption bridge retains the old Waiting run.
    assert_adopting(&host, parent);
    let listener =
        tokio::net::UnixListener::bind(dir.sockets().join("mailbox-adoption.sock")).unwrap();
    let mut driver = LifecycleDriver::new(&host);
    {
        let running = driver.run_with_clock(
            &host,
            backend(&host),
            listener,
            false,
            None,
            signals(),
            &clock,
        );
        tokio::pin!(running);
        assert_pending(running.as_mut()).await;
        clock.advance(grace * 2);
        assert!(
            idle.update_at(start + grace * 2, false, 0, host.busy())
                .is_none()
        );
        assert_pending(running.as_mut()).await;
        assert_adopting(&host, parent);
        resume.send(()).unwrap();
        model.entered.notified().await;
        assert!(!host.runtime.is_adopting_user_input(parent));
        assert!(host.busy(), "published Running must still retain the core");
        assert_pending(running.as_mut()).await;
        let (passive_reached, passive_resume) = install_passive_wait(&host.runtime, parent);
        model.release.notify_one();
        waiting(&mut events, parent).await;
        passive_reached.await.unwrap();
        assert!(host.runtime.is_between_chat_turns(parent));
        assert!(!host.busy());
        running.await.unwrap();
        passive_resume.send(()).unwrap();
    }
    host.runtime
        .stop(parent, runtime::StopScope::SelfOnly)
        .unwrap();
    host.runtime.wait(parent).await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_op_wake_release_rechecks_idle_without_a_phase_event() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let model = Arc::new(ChatModel::default());
    let mut host = open_with_model(&dir, model.clone()).await;
    host.config.core.idle_grace_secs = 0;
    let mut events = host.runtime.event_bus().subscribe();
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "passive chat".into(),
        interactive(),
    );
    waiting(&mut events, run).await;
    let passive = hold_passive_wait(&host.runtime, run).await;
    assert!(host.runtime.is_between_chat_turns(run));
    assert!(!host.busy());
    let (reached, held) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    host.runtime
        .gate_ignored_mailbox_wake(run, reached, resumed)
        .unwrap();
    passive.send(()).unwrap();
    held.await.unwrap();
    assert_adopting(&host, run);
    let clock = ManualClock::new(Instant::now());
    let listener =
        tokio::net::UnixListener::bind(dir.sockets().join("ignored-adoption.sock")).unwrap();
    let mut driver = LifecycleDriver::new(&host);
    {
        let running = driver.run_with_clock(
            &host,
            backend(&host),
            listener,
            false,
            None,
            signals(),
            &clock,
        );
        tokio::pin!(running);
        // Exhaust every existing event/permit before releasing the no-op guard.
        assert_pending(running.as_mut()).await;
        clock.advance(Duration::from_secs(120));
        assert_pending(running.as_mut()).await;
        assert_adopting(&host, run);
        let (passive_reached, passive_resume) = install_passive_wait(&host.runtime, run);
        resume.send(()).unwrap();
        passive_reached.await.unwrap();
        assert!(!host.runtime.is_adopting_user_input(run));
        assert!(host.runtime.is_between_chat_turns(run));
        assert!(!host.busy());
        assert_eq!(
            model.turns.load(Ordering::SeqCst),
            1,
            "no-op must not start a turn"
        );
        // No phase event or timer can wake this previously busy driver. Await
        // lifecycle completion, allowing arbitrary executor/cooperative yields;
        // the release Notify alone must make idle arbitration progress.
        running.await.unwrap();
        passive_resume.send(()).unwrap();
    }
    host.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    host.runtime.wait(run).await.unwrap();
    // The explicit stop supplies a terminal event, not a timeout proving absence.
    // Until then, the message-free wake must have published no phase at all.
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            assert!(
                matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Stopped | AgentRunPhase::Error
                ),
                "no-op wake unexpectedly published {to:?}"
            );
            break;
        }
    }
    host.shutdown().await.unwrap();
}
