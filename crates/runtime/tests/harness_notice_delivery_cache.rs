//! Append-only CACHE contract for the real durable PR input path.
//! The HTTP mock computes cache hits from inputs; assertions independently compare wire bytes.
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, ProviderEvent};
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use runtime::lane_delivery::{
    LaneDeliveryHost, LaneDeliveryOutcome, LaneDeliveryTarget, prepare_pr_notice_payload,
};
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RunId, RuntimeComposition, compose_runtime,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use storage::{lane_delivery::LaneDeliveryNack, pull_request_auto_fix::*, pull_requests::*};
use tokio::sync::{Notify, watch};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};
const MODEL: &str = "mock-model";
const THREAD: &str = "harness-cache";
#[derive(Default)]
struct Gate {
    started: Notify,
    release: Notify,
}
#[async_trait::async_trait]
impl Tool for Gate {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ToolResult::success("unchanged tool result"))
    }
}
struct Host {
    target: LaneDeliveryTarget,
    resolves: watch::Sender<usize>,
    restores: AtomicUsize,
}
impl LaneDeliveryHost for Host {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if thread != THREAD {
            return Err(LaneDeliveryNack::UnknownThread);
        }
        self.resolves.send_modify(|n| *n += 1);
        Ok(self.target.clone())
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|_| LaneDeliveryNack::RestoreFailed)
    }
    fn now_ns(&self) -> i64 {
        100
    }
}
struct World {
    dir: tempfile::TempDir,
    runtime: AgentRuntime,
    mock: StreamingMockOpenAi,
    receiver: EventReceiver,
    store: storage::Storage,
    config: storage::StorageConfig,
    cached: Vec<u64>,
    gate: Arc<Gate>,
    run: RunId,
    host: Arc<Host>,
    authority: RunConfig,
}
fn text(s: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream("response", MODEL, [s])
}
impl World {
    async fn new(done: bool) -> Self {
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
        let gate = Arc::new(Gate::default());
        let mut executor = ToolExecutor::new(bus.clone());
        executor.register(gate.clone()).unwrap();
        let runtime = compose_runtime(RuntimeComposition {
            config: &config,
            bus,
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
            interactive: !done,
            keep_alive: !done,
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
        let host = Arc::new(Host {
            target: LaneDeliveryTarget {
                root_run_id: run,
                generation: 1,
                authority: authority.clone(),
            },
            resolves: watch::channel(0).0,
            restores: AtomicUsize::new(0),
        });
        let mut w = Self {
            dir,
            runtime,
            mock,
            receiver,
            store,
            config: storage_config,
            cached: Vec::new(),
            gate,
            run,
            host,
            authority,
        };
        w.phase(if done {
            AgentRunPhase::Done
        } else {
            AgentRunPhase::Waiting
        })
        .await;
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
    fn ready(&self) -> PrNotice {
        let c = rusqlite::Connection::open(&self.config.db_path).unwrap();
        c.execute("INSERT OR IGNORE INTO threads(thread_id,archived,created_at_ns,updated_at_ns,source) VALUES (?1,0,1,1,'runtime')",[THREAD]).unwrap();
        let key = PullRequestKey::github("owner/repo", 42).unwrap();
        self.store
            .handle()
            .link_pull_request(LinkPullRequest {
                thread_id: THREAD.into(),
                key: key.clone(),
                expected_revision: 0,
                at_ns: 1,
            })
            .unwrap();
        let n = self
            .store
            .handle()
            .reserve_pr_notice(PrNoticeReservation {
                key,
                head_sha: "a".repeat(40),
                mode: PrNoticeMode::Manual,
                destination_thread_id: THREAD.into(),
                link_revision: 1,
                expected_policy_revision: 0,
                success_seq: 1,
                auto_fix_enabled: true,
                now_ns: 100,
            })
            .unwrap();
        let log = runtime::pull_requests::log_source::CiLogBundle::refused(
            &n.head_sha,
            runtime::pull_requests::log_source::LogSkipReason::LogEmpty,
        );
        let body = prepare_pr_notice_payload(
            &n,
            &["unit: failing".into()],
            "2026-01-01T00:00:00Z",
            Some(&log),
        );
        self.store
            .handle()
            .ready_pr_notice(n.notice_id, body)
            .unwrap()
    }
    fn requests(&self) -> Vec<Value> {
        self.mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .map(|r| r.body)
            .collect()
    }
    fn verify(&self) {
        let requests = self.requests();
        assert!(requests.len() >= 3);
        for pair in requests.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
            let old = pair[0]["messages"].as_array().unwrap();
            let new = pair[1]["messages"].as_array().unwrap();
            assert_eq!(&new[..old.len()], old);
            assert_eq!(pair[0]["tools"], pair[1]["tools"]);
            assert_eq!(pair[0]["prompt_cache_key"], pair[1]["prompt_cache_key"]);
            assert_eq!(pair[0]["model"], pair[1]["model"]);
        }
        let first_notice = requests
            .iter()
            .position(|r| {
                r.to_string()
                    .contains("[harness-notice kind=pr_checks_failed")
            })
            .expect("durable delivery must actually append the notice");
        let first_messages = requests[first_notice]["messages"].as_array().unwrap();
        assert!(
            first_messages
                .last()
                .unwrap()
                .to_string()
                .contains("[harness-notice kind=pr_checks_failed"),
            "the new notice is appended at the tail"
        );
        for request in requests.iter().skip(first_notice) {
            let messages = request["messages"].as_array().unwrap();
            let notices: Vec<_> = messages
                .iter()
                .filter(|m| {
                    m.to_string()
                        .contains("[harness-notice kind=pr_checks_failed")
                })
                .collect();
            assert_eq!(
                notices.len(),
                1,
                "one append, even after ACK repair or next human turn"
            );
            assert!(notices[0]["content"].as_str().unwrap().len() <= 16 * 1024);
            assert!(
                notices[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("<ci-log-data>")
            );
        }
        assert_eq!(self.cached.len(), requests.len());
        assert_eq!(self.cached[0], 0);
        assert!(
            self.cached.iter().skip(1).all(|n| *n > 0),
            "input-derived cache must reuse every old prefix: {:?}",
            self.cached
        );
    }
}
#[tokio::test]
async fn live_waiting_done_and_ack_repair_append_once_and_reuse_the_next_prefix() {
    for (done, live, ack_failure) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (false, false, true),
    ] {
        eprintln!("scenario done={done} live={live} ack_failure={ack_failure}");
        let mut w = World::new(done).await;
        let n = w.ready();
        if ack_failure {
            rusqlite::Connection::open(&w.config.db_path).unwrap().execute_batch("CREATE TRIGGER fail_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' AND NEW.source_kind='pull_request_checks_failed' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        }
        if live {
            w.mock.enqueue(vec![
                ScriptedResponse::tool_call("tool", MODEL, 0, "read-gate", "read", ["{}"]),
                text("handled notice"),
            ]);
            w.runtime
                .send_message(w.run, "do the next tool boundary".into())
                .unwrap();
            w.gate.started.notified().await;
        } else {
            w.mock.enqueue(vec![if done {
                ScriptedResponse::tool_call(
                    "finish",
                    MODEL,
                    0,
                    "done",
                    "finish",
                    [json!({"result":"handled notice"}).to_string()],
                )
            } else {
                text("handled notice")
            }]);
        }
        // Bind only after any live tool is blocked. Real-user scheduling cannot
        // race this explicit driver and consume the prepared notice first.
        if let Some(task) = w.runtime.bind_lane_delivery_host(w.host.clone()) {
            task.abort();
        }
        let runtime = w.runtime.clone();
        let mut resolves = w.host.resolves.subscribe();
        let delivery = tokio::spawn(async move { runtime.drive_lane_deliveries(THREAD).await });
        if live {
            // On this current-thread executor, the second resolve and queue insertion
            // are synchronous up to the driver's receipt await. No wall-time wait.
            eprintln!("live tool blocked; awaiting queue");
            resolves.wait_for(|n| *n >= 2).await.unwrap();
            w.gate.release.notify_one();
        }
        eprintln!("driver queued; awaiting receipt");
        let result = delivery.await.unwrap();
        eprintln!("delivery result: {result:?}");
        if ack_failure {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result.unwrap(),
                LaneDeliveryOutcome::Delivered { notifications: 1 }
            );
        }
        w.phase(if done {
            AgentRunPhase::Done
        } else {
            AgentRunPhase::Waiting
        })
        .await;
        assert_eq!(
            w.store
                .handle()
                .pr_notice(n.notice_id)
                .unwrap()
                .unwrap()
                .phase,
            PrNoticePhase::Adopted
        );
        assert_eq!(w.host.restores.load(Ordering::SeqCst), usize::from(done));
        if ack_failure {
            let before = w.requests();
            rusqlite::Connection::open(&w.config.db_path)
                .unwrap()
                .execute_batch("DROP TRIGGER fail_ack")
                .unwrap();
            w.runtime.drive_pending_lane_deliveries().await.unwrap();
            assert_eq!(before, w.requests());
        }
        w.mock.enqueue(vec![text("next real turn")]);
        if done {
            w.runtime
                .continue_goal(w.run, "new human followup".into(), w.authority.clone())
                .unwrap();
        } else {
            w.runtime
                .send_message(w.run, "new human followup".into())
                .unwrap();
        }
        // A real follow-up restores an interactive chat, even after Done.
        w.phase(AgentRunPhase::Waiting).await;
        w.verify();
        w.runtime.cancel(w.run).unwrap();
        w.runtime.wait(w.run).await.unwrap();
        assert!(w.dir.path().is_dir());
    }
}
