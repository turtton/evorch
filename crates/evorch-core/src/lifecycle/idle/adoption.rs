use super::*;
use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct ChatModel {
    turns: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl runtime::AgentModel for ChatModel {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        if self.turns.fetch_add(1, Ordering::SeqCst) != 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "completed turn".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "idle-adoption".into()
    }
}
async fn waiting(events: &mut event_bus::EventReceiver, run: runtime::RunId) {
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            if to == AgentRunPhase::Waiting {
                return;
            }
            assert!(
                !matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Stopped | AgentRunPhase::Error
                ),
                "unexpected terminal phase: {to:?}"
            );
        }
    }
}

// The synchronous claim gate holds one worker; the other drives the lifecycle.
// Time is supplied to IdleGrace explicitly, not measured or used as a timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claimed_input_retains_core_even_with_no_clients_and_zero_grace() {
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
        "first turn".into(),
        runtime::RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    waiting(&mut events, run).await;
    let passive = hold_passive_wait(&host.runtime, run).await;
    assert!(host.runtime.is_between_chat_turns(run));
    assert!(!host.busy());

    let start = Instant::now();
    let clock = clock::ManualClock::new(start);
    let grace = Duration::from_secs(60);
    let mut idle = IdleGrace::new(grace);
    assert!(idle.update_at(start, false, 1, host.busy()).is_none());
    assert_eq!(
        idle.update_at(start, false, 0, host.busy()),
        Some(start + grace)
    );
    let (reached, held) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    host.runtime
        .gate_next_user_input_adoption(run, reached, resumed)
        .unwrap();
    host.runtime
        .send_user_input(run, "accepted next turn".into(), None)
        .unwrap();
    passive.send(()).unwrap();
    held.await.unwrap();
    assert_eq!(host.runtime.follow_up_status(run).unwrap().pending, 0);
    assert!(host.runtime.is_adopting_user_input(run));
    assert!(!host.runtime.is_between_chat_turns(run));
    assert!(host.runtime.has_active_runs());
    assert!(host.busy());

    // Even the previous grace deadline is not authority to exit while adopting.
    let advanced = start + grace * 2;
    assert!(idle.update_at(advanced, false, 0, host.busy()).is_none());
    let mut immediate = IdleGrace::new(Duration::ZERO);
    assert!(
        immediate
            .update_at(advanced, false, 0, host.busy())
            .is_none()
    );
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("adoption.sock")).unwrap();
    let backend = Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "adopting-core".into(),
    ));
    let mut driver = LifecycleDriver::new(&host);
    {
        let running =
            driver.run_with_clock(&host, backend, listener, false, None, signals(), &clock);
        tokio::pin!(running);
        assert_pending(running.as_mut()).await;
        clock.advance(grace * 2);
        assert_pending(running.as_mut()).await;
        assert!(host.runtime.is_adopting_user_input(run));
        resume.send(()).unwrap();
        model.entered.notified().await;
        assert!(!host.runtime.is_adopting_user_input(run));
        assert!(host.busy(), "published Running still retains the core");
        assert_pending(running.as_mut()).await;
        let (passive_reached, passive_resume) = install_passive_wait(&host.runtime, run);
        model.release.notify_one();
        waiting(&mut events, run).await;
        passive_reached.await.unwrap();
        assert!(host.runtime.is_between_chat_turns(run));
        assert!(!host.busy());
        let disconnected = advanced + grace;
        assert_eq!(
            idle.update_at(disconnected, false, 0, host.busy()),
            Some(disconnected + grace),
            "adoption reset the old grace origin"
        );
        running.await.unwrap();
        passive_resume.send(()).unwrap();
    }
    host.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    host.runtime.wait(run).await.unwrap();
    host.shutdown().await.unwrap();
}

// Passive polling has a deliberately transient conservative guard. These test-only
// barriers observe the stable boundary after that guard has dropped, not a race
// between the Waiting event and the next receiver poll.
fn install_passive_wait(
    runtime: &runtime::AgentRuntime,
    run: runtime::RunId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached, held) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    runtime
        .gate_next_passive_input_wait(run, reached, resumed)
        .unwrap();
    (held, resume)
}
async fn hold_passive_wait(
    runtime: &runtime::AgentRuntime,
    run: runtime::RunId,
) -> std::sync::mpsc::Sender<()> {
    let (held, resume) = install_passive_wait(runtime, run);
    held.await.unwrap();
    resume
}
mod clock;
mod wake;
