//! CACHE contract through coordinator, bounded log source, durable outbox and real model input.
use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, ProviderEvent,
    PullRequestEvent, ToolEvent,
};
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use runtime::lane_delivery::{LaneDeliveryHost, LaneDeliveryTarget};
use runtime::ownership::{OwnerHost, OwnerPermit};
use runtime::pull_requests::checkout::Checkout;
use runtime::pull_requests::{
    CheckoutResolution, FakePullRequestLogSource, FakePullRequestSource, FetchedPullRequest,
    PrNoticeDelivery, PullRequestService, ServiceParts, ThreadEnvironment, WallClock,
};
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RunId, RuntimeComposition, compose_runtime,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use storage::{lane_delivery::LaneDeliveryNack, pull_request_auto_fix::*, pull_requests::*};
use tokio::sync::{Notify, watch};
use tools::ToolExecutor;
const MODEL: &str = "mock-model";
const THREAD: &str = "pr-auto-cache";
const MARKER: &str = "[harness-notice kind=pr_checks_failed";
fn key() -> PullRequestKey {
    PullRequestKey::github("owner/repo", 42).unwrap()
}
#[derive(Default)]
struct Clock(AtomicI64);
impl WallClock for Clock {
    fn now_ns(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct Environment(Mutex<String>);
impl ThreadEnvironment for Environment {
    fn thread_of_run(&self, _: &str) -> Option<String> {
        Some(THREAD.into())
    }
    fn checkout_directories(&self, _: &str) -> Vec<PathBuf> {
        vec![PathBuf::from("/checkout")]
    }
    fn resolve_checkout(
        &self,
        _: &[PathBuf],
        _: Option<&str>,
        _: &str,
        _: Duration,
    ) -> CheckoutResolution {
        CheckoutResolution::Found(Checkout {
            path: PathBuf::from("/checkout"),
            head_sha: self.0.lock().unwrap().clone(),
            remote: "origin".into(),
        })
    }
}
struct Host {
    target: LaneDeliveryTarget,
    clock: Arc<Clock>,
}
impl LaneDeliveryHost for Host {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        assert_eq!(thread, THREAD);
        Ok(self.target.clone())
    }
    fn restore_root(
        &self,
        _: &AgentRuntime,
        _: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        panic!("the root is Waiting, not Done")
    }
    fn now_ns(&self) -> i64 {
        self.clock.now_ns()
    }
}
struct Delivery {
    runtime: AgentRuntime,
    ready: Notify,
}
impl PrNoticeDelivery for Delivery {
    fn schedule(&self, thread: &str) {
        self.ready.notify_one();
        self.runtime.schedule_lane_deliveries(thread);
    }
    fn subscribe_resumes(&self) -> watch::Receiver<u64> {
        self.runtime.subscribe_harness_delivery_resumes()
    }
}
struct World {
    _dir: tempfile::TempDir,
    runtime: AgentRuntime,
    mock: StreamingMockOpenAi,
    receiver: EventReceiver,
    updates: EventReceiver,
    bus: Arc<EventBus>,
    store: storage::Storage,
    cached: Vec<u64>,
    run: RunId,
    service: PullRequestService,
    source: FakePullRequestSource,
    logs: FakePullRequestLogSource,
    clock: Arc<Clock>,
    environment: Arc<Environment>,
    delivery: Arc<Delivery>,
    _owner: Arc<OwnerHost>,
    permit: OwnerPermit,
}
fn text(s: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream("response", MODEL, [s])
}
impl World {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
            vec![text("initial answer")],
            vec![MODEL.into()],
        );
        std::fs::create_dir_all(dir.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
        std::fs::write(
            config::project_main_config_path(dir.path()),
            format!(
                r#"
[providers.local]
type="openai-compatible"
base_url="{}"
api_key_env="PR_CACHE_TEST_KEY"
models=["{MODEL}"]
default_model="{MODEL}"
[[routing.routes.orchestrator]]
profile="local"
[compaction]
context_window_tokens=1000000
threshold=0.95
"#,
                mock.base_url()
            ),
        )
        .unwrap();
        let config = config::Config::load(&config::LoadOptions {
            project_dir: Some(dir.path().into()),
            user_config_dir: Some(dir.path().join(config::PROJECT_CONFIG_DIR)),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        let bus = Arc::new(EventBus::new(4096));
        let receiver = bus.subscribe();
        let executor = ToolExecutor::new(bus.clone());
        let runtime = compose_runtime(RuntimeComposition {
            config: &config,
            bus: bus.clone(),
            executor: Arc::new(executor),
            workspace: None,
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(dir.path().join("credentials"))
                    .unwrap(),
            ),
            env: Arc::new(routing::MapEnv::from_iter([(
                "PR_CACHE_TEST_KEY",
                "offline-test",
            )])),
            model_source: ModelSource::Configured,
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            user_config_dir: Some(dir.path().join("empty-user")),
        })
        .unwrap()
        .runtime;
        let storage_config = storage::StorageConfig {
            db_path: dir.path().join("cache.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(storage_config.clone()).unwrap();
        let runtime = runtime
            .with_run_store(runtime::RunStore::open(&storage_config, store.handle()).unwrap());
        let authority = RunConfig {
            interactive: true,
            keep_alive: true,
            conversation: true,
            ..Default::default()
        };
        let run = runtime
            .delegate_chat(
                THREAD,
                Role::Orchestrator,
                "coordinate and keep the immutable prefix. ".repeat(1000),
                authority.clone(),
            )
            .unwrap();

        let clock = Arc::new(Clock(AtomicI64::new(1_000_000_000)));
        let host = Arc::new(Host {
            target: LaneDeliveryTarget {
                root_run_id: run,
                generation: 1,
                authority,
            },
            clock: clock.clone(),
        });
        if let Some(task) = runtime.bind_lane_delivery_host(host) {
            task.abort();
        }
        let owner = Arc::new(
            OwnerHost::open(
                &dir.path().join("owners"),
                config::OwnershipConfig::default(),
                bus.clone(),
            )
            .unwrap(),
        );
        let permit = owner.start(THREAD).unwrap();
        let environment = Arc::new(Environment(Mutex::new("a".repeat(40))));
        let source = FakePullRequestSource::new();
        let logs = FakePullRequestLogSource::default();
        let delivery = Arc::new(Delivery {
            runtime: runtime.clone(),
            ready: Notify::new(),
        });
        let service = PullRequestService::start_with_notice_delivery(
            ServiceParts {
                storage: store.handle(),
                source: Arc::new(source.clone()),
                ownership: owner.clone(),
                environment: environment.clone(),
                bus: bus.clone(),
                settings: config::PullRequestsConfig {
                    enabled: true,
                    ..Default::default()
                },
                clock: clock.clone(),
            },
            Arc::new(logs.clone()),
            delivery.clone(),
        )
        .unwrap();
        let updates = bus.subscribe();
        let mut w = Self {
            _dir: dir,
            runtime,
            mock,
            receiver,
            updates,
            bus,
            store,
            cached: Vec::new(),
            run,
            service,
            source,
            logs,
            clock,
            environment,
            delivery,
            _owner: owner,
            permit,
        };
        w.phase(AgentRunPhase::Waiting).await;
        let gate = w.source.push_gate(&key());
        w.service
            .link(
                THREAD,
                "https://github.com/owner/repo/pull/42",
                0,
                &w.permit,
            )
            .await
            .unwrap();
        gate.arrived().await;
        gate.release_ok(w.answer('a', false));
        w.changed().await;
        w
    }
    async fn phase(&mut self, expected: AgentRunPhase) {
        eprintln!("waiting for {expected:?}");
        loop {
            let e = self.receiver.recv().await.unwrap();
            match e.kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    run_id: Some(id),
                    cache_read_tokens,
                    ..
                }) if id == self.run.to_string() => self.cached.push(cache_read_tokens),
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to,
                    reason,
                    ..
                }) if run_id == self.run.to_string() => {
                    assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
                    if to == expected {
                        return;
                    }
                }
                EventKind::Diagnostic(d) => assert_ne!(d.code, "CacheRegression", "{d:?}"),
                EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { .. }) => {
                    panic!("PR delivery must not use RootWakeRequested")
                }
                _ => {}
            }
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .map(|r| r.body)
            .collect()
    }

    async fn changed(&mut self) {
        loop {
            if matches!(self.updates.recv().await.unwrap().kind,EventKind::PullRequest(PullRequestEvent::Changed{ref thread_id,..}) if thread_id==THREAD)
            {
                return;
            }
        }
    }
    fn answer(&self, head: char, pending: bool) -> FetchedPullRequest {
        let sha = head.to_string().repeat(40);
        let mut checks = vec![PullRequestCheck {
            provider_id: "unit".into(),
            kind: CheckKind::CheckRun,
            name: "unit".into(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            head_sha: sha.clone(),
        }];
        if pending {
            checks.push(PullRequestCheck {
                provider_id: "pending".into(),
                kind: CheckKind::CheckRun,
                name: "pending".into(),
                status: CheckStatus::InProgress,
                conclusion: None,
                head_sha: sha.clone(),
            });
        }
        FetchedPullRequest {
            requested: key(),
            canonical: key(),
            url: "https://github.com/owner/repo/pull/42".into(),
            display: PullRequestDisplay {
                title: DisplayTitle::from_untrusted("display only"),
            },
            facts: PullRequestFacts {
                state: PullRequestState::Open,
                base_branch: "main".into(),
                head_repository: Some("owner/repo".into()),
                head_branch: "feature".into(),
                head_sha: sha,
                checks,
                checks_complete: true,
                review_decision: ReviewDecision::None,
            },
            fetched_at_ns: self.clock.now_ns(),
        }
    }
    async fn toggle(&self, enabled: bool) {
        let p = self.service.auto_fix_state(THREAD).await.unwrap();
        self.service
            .set_auto_fix(THREAD, 1, p.policy_revision, enabled, &self.permit)
            .await
            .unwrap();
    }
    fn publish(&self, head: char) {
        let call = format!("push-{head}");
        self.bus.emit(Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: call.clone(),
            input: Some(json!({"command":"git -c 'credential.helper=!gh auth git-credential' push origin feature"})),
            run_id: Some(self.run.to_string()),
            parent_call_id: None,
        }));
        self.bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: call,
            is_error: false,
            output: None,
            detail: Some(json!({"exit_code":0})),
            run_id: Some(self.run.to_string()),
            parent_call_id: None,
        }));
    }
    async fn fetch(&mut self, head: char, pending: bool, publishing: bool) {
        *self.environment.0.lock().unwrap() = head.to_string().repeat(40);
        let gate = self.source.push_gate(&key());
        if publishing {
            self.publish(head);
        } else {
            self.service.refresh(THREAD, &self.permit).await.unwrap();
        }
        gate.arrived().await;
        gate.release_ok(self.answer(head, pending));
        self.changed().await;
    }
    fn notices(&self) -> Vec<PrNotice> {
        self.store
            .handle()
            .pr_notices(key(), String::new())
            .unwrap()
    }
    async fn silent_turn(&mut self, expected_notices: usize) {
        let before = self.requests();
        self.runtime.drive_pending_lane_deliveries().await.unwrap();
        assert_eq!(
            self.requests(),
            before,
            "evaluation and empty delivery must not call the model"
        );
        assert_eq!(self.notices().len(), expected_notices);
        assert_eq!(self.logs.requests().len(), expected_notices);
        self.mock.enqueue(vec![text("ordinary answer")]);
        self.runtime
            .send_message(self.run, "ordinary human followup".into())
            .unwrap();
        self.phase(AgentRunPhase::Waiting).await;
        let after = self.requests();
        assert_eq!(after.len(), before.len() + 1);
        let old = before.last().unwrap()["messages"].as_array().unwrap();
        let new = after.last().unwrap()["messages"].as_array().unwrap();
        assert_eq!(
            new.len(),
            old.len() + 2,
            "only the previous answer and human followup were appended"
        );
        assert_eq!(&new[..old.len()], old);
        assert_eq!(new.last().unwrap()["content"], "ordinary human followup");
    }
    async fn notified(&mut self, expected: usize) {
        self.delivery.ready.notified().await;
        self.phase(AgentRunPhase::Waiting).await;
        // Driver completion is the durable ACK boundary, not the earlier lifecycle event.
        self.runtime.drive_pending_lane_deliveries().await.unwrap();
        let notices = self.notices();
        assert_eq!(notices.len(), expected);
        assert!(notices.iter().all(|n| n.phase == PrNoticePhase::Adopted));
        let request = self.requests().pop().unwrap();
        let last = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(last.matches(MARKER).count(), 1, "C owns the single wrapper");
        assert!(last.contains("<ci-log-data>"));
    }
    fn verify(&self) {
        let requests = self.requests();
        for pair in requests.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
            let old = pair[0]["messages"].as_array().unwrap();
            let new = pair[1]["messages"].as_array().unwrap();
            assert_eq!(&new[..old.len()], old);
            for field in ["tools", "prompt_cache_key", "model"] {
                assert_eq!(pair[0][field], pair[1][field]);
            }
        }
        assert_eq!(self.cached.len(), requests.len());
        assert_eq!(self.cached[0], 0);
        assert!(
            self.cached.iter().skip(1).all(|n| *n > 0),
            "input-derived cache hits: {:?}",
            self.cached
        );
    }
}
#[tokio::test]
async fn off_unknown_pending_and_cooldown_are_silent_then_notices_append_and_reuse_prefix() {
    let mut w = World::new().await;
    // Initial baseline has a matching local HEAD, but no publishing success: unknown.
    w.toggle(true).await;
    w.silent_turn(0).await;
    w.toggle(false).await;
    w.fetch('b', true, true).await;
    w.silent_turn(0).await;
    let mut settings = w.service.settings();
    settings.auto_fix_enabled = false;
    w.service.update_settings(settings).unwrap();
    w.toggle(true).await;
    w.silent_turn(0).await;
    let mut settings = w.service.settings();
    settings.auto_fix_enabled = true;
    w.service.update_settings(settings).unwrap();
    w.toggle(true).await;
    w.silent_turn(0).await;
    // Same attributed head: pending -> complete. The coordinator reserves and fetches logs.
    w.mock.enqueue(vec![text("handled first CI failure")]);
    w.fetch('b', false, false).await;
    w.notified(1).await;
    let until = w
        .service
        .auto_fix_state(THREAD)
        .await
        .unwrap()
        .cooldown_until_ns
        .unwrap();
    assert_eq!(until - w.clock.now_ns(), 600_000_000_000);
    w.clock.0.store(until - 1_000_000_000, Ordering::SeqCst);
    w.fetch('c', false, true).await;
    w.toggle(true).await;
    w.silent_turn(1).await;
    // At exactly 600 seconds, reevaluate only the newest head, never a queue of old heads.
    w.clock.0.store(until, Ordering::SeqCst);
    w.mock.enqueue(vec![text("handled latest CI failure")]);
    w.toggle(true).await;
    w.notified(2).await;
    w.silent_turn(2).await;
    w.verify();
    w.service.shutdown();
    w.runtime.cancel(w.run).unwrap();
    w.runtime.wait(w.run).await.unwrap();
}
