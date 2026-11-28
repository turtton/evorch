//! Stop success is the durable boundary, including an admission-pending lane.
//! The parent SIGKILLs at the host reply, not after teardown or a grace period.
#![cfg(unix)]
use event_bus::AgentRunPhase;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentModel, Role, RunConfig, RunId, RuntimeError};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};
use workbench_host::{
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams},
    commands::{HostCommand, HostEvent},
};

#[derive(Default)]
struct Model {
    lead: Mutex<Option<String>>,
    pending: AtomicBool,
    running: AtomicBool,
    entered: tokio::sync::Notify,
    calls: AtomicUsize,
}
impl Model {
    fn lane(&self, run: &str) -> bool {
        self.lead
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|lead| lead != run)
    }
}
#[async_trait::async_trait]
impl AgentModel for Model {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(
        &self,
        ctx: &runtime::AgentInvocationContext,
        _: Role,
    ) -> Result<(), RuntimeError> {
        if self.lane(&ctx.run_id) && self.pending.load(Ordering::SeqCst) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "stop-boundary".into()
    }
    async fn complete(
        &self,
        ctx: &runtime::AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.lane(&ctx.run_id) && self.running.load(Ordering::SeqCst) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "ready".into(),
                }],
            },
            finish_reason: FinishReason::Stop,
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
async fn open(config: &Path, root: &Path, model: Arc<Model>) -> BootstrappedHost {
    BootstrappedHost::open_for_core_with_test_seams(
        BootstrapOptions {
            config_dir: config.into(),
            project_root: config.into(),
            ownership_root: root.into(),
        },
        TestSeams {
            model,
            sandbox: Arc::new(Direct),
        },
        &evorch_core::recovery_service::CoreGraphRecovery,
    )
    .await
    .unwrap()
}
async fn waiting(host: &BootstrappedHost, run: RunId) {
    let mut events = host.runtime.event_bus().subscribe();
    loop {
        match host.runtime.inspect_agent(run) {
            Ok(agent) => {
                if agent.phase == AgentRunPhase::Waiting {
                    return;
                }
                assert!(
                    !matches!(
                        agent.phase,
                        AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                    ),
                    "{agent:?}"
                );
            }
            Err(RuntimeError::UnknownRun { .. }) => (),
            Err(error) => panic!("{error}"),
        }
        events.recv().await.unwrap();
    }
}

async fn prepared_lane(
    host: &BootstrappedHost,
    config: &Path,
    model: &Model,
    pending: bool,
) -> runtime::lanes::LaneOpened {
    let lead = host
        .runtime
        .delegate_chat(
            "stop-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                project_root: Some(config.into()),
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    waiting(host, lead).await;
    *model.lead.lock().unwrap() = Some(lead.to_string());
    model.pending.store(pending, Ordering::SeqCst);
    model.running.store(!pending, Ordering::SeqCst);
    let lane = host.runtime.open_lane(lead, "stop-lane", serde_json::from_value(json!({
        "objective":"durable Stop", "inputs":[], "deliverables":["patch"],
        "acceptance_criteria":["review"], "constraints":[], "workspace":{}, "report_policy":"attached"
    })).unwrap()).unwrap();
    // Admission / provider entered, independently of timers or the lane's phase label.
    model.entered.notified().await;
    lane
}

fn submit_stop(host: &BootstrappedHost, lane: &runtime::lanes::LaneOpened) -> Vec<HostEvent> {
    let mut command = host.host.lock().unwrap();
    command
        .service_mut()
        .bind_goal_context(&lane.thread_id, "p", &lane.root_run_id.to_string());
    command.submit_local(
        HostCommand::StopChat {
            thread_id: lane.thread_id.clone(),
        },
        &workbench_host::provenance::local_desktop(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "private subprocess entry point"]
async fn durable_stop_subprocess() {
    let config = std::path::PathBuf::from(std::env::var_os("STOP_CONFIG").unwrap());
    let root = std::path::PathBuf::from(std::env::var_os("STOP_ROOT").unwrap());
    let pending = std::env::var("STOP_PENDING").unwrap() == "true";
    let model = Arc::new(Model::default());
    let host = open(&config, &root, model.clone()).await;
    let lane = prepared_lane(&host, &config, &model, pending).await;
    let events = submit_stop(&host, &lane);
    assert!(
        matches!(events.as_slice(), [HostEvent::ChatStopped { run_id, .. }] if run_id == &lane.root_run_id.to_string()),
        "Stop must not report success before both writes: {events:?}"
    );
    println!(
        "DURABLE_STOP {}",
        json!({"lane":lane.lane_id,"thread":lane.thread_id,"run":lane.root_run_id.to_string()})
    );
    std::io::stdout().flush().unwrap();
    std::future::pending::<()>().await;
}

async fn stop_ack_then_kill(pending: bool) {
    let dir = test_socket_dir::SplitDir::new();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "durable_stop_subprocess",
            "--nocapture",
        ])
        .env("STOP_CONFIG", dir.path())
        .env("STOP_ROOT", dir.sockets())
        .env("STOP_PENDING", pending.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let saved: Value = loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("child exited before durable Stop reply");
        if let Some(value) = line.strip_prefix("DURABLE_STOP ") {
            break serde_json::from_str(value).unwrap();
        }
    };
    child.start_kill().unwrap();
    assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
    let run: RunId = saved["run"].as_str().unwrap().parse().unwrap();
    let thread = saved["thread"].as_str().unwrap();
    let lane_id = saved["lane"].as_str().unwrap();
    let model = Arc::new(Model::default());
    let mut host = open(dir.path(), dir.sockets(), model.clone()).await;
    let writer = &host.composition.storage_writer;
    let lane = writer.lane(lane_id).unwrap().unwrap();
    if pending {
        assert!(lane.is_operator_stopped(), "{lane:?}");
    }
    let destination = writer
        .lane_delivery_destination(thread.into())
        .unwrap()
        .unwrap();
    assert!(destination.stopped);
    assert!(host.runtime.recover_unstarted_lanes().unwrap().is_empty());
    assert!(
        host.runtime.inspect_agent(run).is_err(),
        "cold startup must not reactivate the stopped run"
    );
    assert_eq!(
        writer
            .claim_lane_deliveries(storage::lane_delivery::LaneClaimRequest {
                destination_thread_id: thread.into(),
                expected_destination_epoch: destination.epoch,
                owner: destination.owner.unwrap(),
                token: "post-kill-wake-and-ack-repair".into(),
                now_ns: i64::MAX - 1,
                lease_until_ns: i64::MAX,
                allow_non_waking: true,
            })
            .unwrap(),
        storage::lane_delivery::LaneClaimResult::Stopped
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "no resumed provider request"
    );
    assert!(!dir.sockets().join("owners.db").exists());
    host.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_success_then_sigkill_does_not_start_an_admission_pending_lane() {
    stop_ack_then_kill(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_success_then_sigkill_does_not_wake_a_running_lane() {
    stop_ack_then_kill(false).await;
}

async fn failed_stop_then_retry(start_write: bool) {
    let dir = test_socket_dir::SplitDir::new();
    let model = Arc::new(Model::default());
    let mut host = open(dir.path(), dir.sockets(), model.clone()).await;
    let lane = prepared_lane(&host, dir.path(), &model, start_write).await;
    let connection = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    connection.execute_batch(if start_write {
        "CREATE TRIGGER fail_stop BEFORE UPDATE OF start_blocked_reason ON lanes WHEN NEW.start_blocked_reason='operator_stop: stopped before root start' BEGIN SELECT RAISE(FAIL, 'injected start Stop failure'); END;"
    } else {
        "CREATE TRIGGER fail_stop BEFORE UPDATE ON lane_delivery_destinations WHEN NEW.stopped=1 BEGIN SELECT RAISE(FAIL, 'injected destination Stop failure'); END;"
    }).unwrap();
    let rejected = submit_stop(&host, &lane);
    assert!(
        matches!(rejected.as_slice(), [HostEvent::ChatRejected { reason, .. }] if reason.contains("injected")),
        "failed write must not be a success reply: {rejected:?}"
    );
    if start_write {
        assert!(
            !host
                .composition
                .storage_writer
                .lane(&lane.lane_id)
                .unwrap()
                .unwrap()
                .is_operator_stopped()
        );
        assert!(
            host.runtime.wait(lane.root_run_id).await.is_err(),
            "the blocked admission must still settle as stopped"
        );
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "only the lead made a provider request"
        );
    } else {
        assert_eq!(
            host.runtime.wait(lane.root_run_id).await.unwrap(),
            AgentRunPhase::Stopped
        );
        assert!(host.runtime.graceful_stop_requested(lane.root_run_id));
        assert!(
            host.composition
                .storage_writer
                .lane_delivery_destination(lane.thread_id.clone())
                .unwrap()
                .is_none_or(|destination| !destination.stopped)
        );
    }
    connection.execute_batch("DROP TRIGGER fail_stop").unwrap();
    let retried = submit_stop(&host, &lane);
    assert!(
        matches!(retried.as_slice(), [HostEvent::ChatStopped { .. }]),
        "retry must finish durability even after memory execution stopped: {retried:?}"
    );
    if start_write {
        assert!(
            host.composition
                .storage_writer
                .lane(&lane.lane_id)
                .unwrap()
                .unwrap()
                .is_operator_stopped()
        );
    }
    assert!(
        host.composition
            .storage_writer
            .lane_delivery_destination(lane.thread_id)
            .unwrap()
            .unwrap()
            .stopped
    );
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn failed_start_stop_is_rejected_by_host_and_same_stop_can_be_retried() {
    failed_stop_then_retry(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn failed_destination_stop_is_rejected_by_host_and_terminal_stop_can_be_retried() {
    failed_stop_then_retry(false).await;
}
