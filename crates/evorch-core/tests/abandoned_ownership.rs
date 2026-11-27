//! SIGKILL leaves Running owners, sockets and ordinary (non-manifest) snapshots.
//! Only expiry is adjusted after reap; neither startup recovery nor explicit claim
//! may supply authority. All completion waits use IPC/model/lifecycle signals.
#![cfg(unix)]
use event_bus::AgentRunPhase;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::ownership::{OwnerState, Registry, ThreadOwner};
use runtime::{AgentModel, Role, RunConfig, RunId, RuntimeError};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, BufReader};
use workbench_host::{
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams},
    commands::{ComposerRole, ContinueInput, HostCommand, HostEvent},
};

const TEXT: &str = "abandoned-owner durable supplement";
#[derive(Default)]
struct Model {
    source: bool,
    scripted: Mutex<HashMap<String, Value>>,
    seen: Mutex<HashMap<String, String>>,
    received: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for Model {
    fn requires_admission(&self) -> bool {
        false
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "abandoned-test".into()
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let body = serde_json::to_string(messages).unwrap();
        self.seen
            .lock()
            .unwrap()
            .insert(invocation.run_id.clone(), body.clone());
        if body.contains(TEXT) {
            self.received.notify_one();
        }
        let script = self.scripted.lock().unwrap().remove(&invocation.run_id);
        let tool = if let Some(input) = script {
            Some(("lane_send", input))
        } else if self.source && body.contains("abandoned-Done") {
            Some(("finish", json!({"result":"complete"})))
        } else {
            None
        };
        if self.source && body.contains("abandoned-Error") {
            return Err(RuntimeError::Model {
                reason: "saved lane error fixture".into(),
            });
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![if let Some((name, input)) = &tool {
                    ContentBlock::ToolUse {
                        id: format!("call-{}", messages.len()),
                        name: (*name).into(),
                        input: input.clone(),
                    }
                } else {
                    ContentBlock::Text {
                        text: "idle".into(),
                    }
                }],
            },
            finish_reason: if tool.is_some() {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
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
async fn open(root: &Path, sockets: &Path, model: Arc<Model>) -> BootstrappedHost {
    BootstrappedHost::open_for_core_with_test_seams(
        BootstrapOptions {
            config_dir: root.into(),
            project_root: root.into(),
            ownership_root: sockets.into(),
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
async fn phase(runtime: &runtime::AgentRuntime, run: RunId, wanted: AgentRunPhase) {
    let mut events = runtime.event_bus().subscribe();
    loop {
        if let Ok(agent) = runtime.inspect_agent(run) {
            if agent.phase == wanted {
                return;
            }
            assert_ne!(
                agent.phase,
                AgentRunPhase::Error,
                "unexpected error: {agent:?}"
            );
        }
        events.recv().await.unwrap();
    }
}
fn continue_thread(host: &BootstrappedHost, thread: &str, expected: RunId) {
    let events = host.host.lock().unwrap().submit_local(
        HostCommand::ContinueChat(ContinueInput {
            thread_id: thread.into(),
            project_root: None,
            composer_role: ComposerRole::Orchestrator,
            automatic: false,
        }),
        &workbench_host::provenance::local_desktop(),
    );
    assert!(
        matches!(events.as_slice(), [HostEvent::ChatAccepted { run_id, .. }]
        if run_id == &expected.to_string()),
        "ordinary Continue: {events:?}"
    );
}
fn expire(registry: &mut Registry, thread: &str) -> ThreadOwner {
    registry
        .update(thread, |row| {
            row.lease.expires_at = 0;
            Ok(())
        })
        .unwrap()
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "private subprocess entry point"]
async fn abandoned_subprocess_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("ABANDONED_ROOT").unwrap());
    let sockets = std::path::PathBuf::from(std::env::var_os("ABANDONED_SOCKETS").unwrap());
    let saved = std::env::var("ABANDONED_PHASE").unwrap();
    let host = open(
        &root,
        &sockets,
        Arc::new(Model {
            source: true,
            ..Default::default()
        }),
    )
    .await;
    let permit = host.owner.start("lead").unwrap();
    let lead = host
        .runtime
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "lead conversation".into(),
            RunConfig {
                ownership: Some(permit),
                project_root: Some(root.clone()),
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    phase(&host.runtime, lead, AgentRunPhase::Waiting).await;
    let lane = host.runtime.open_lane(lead, "lane-key", serde_json::from_value(json!({
        "objective":format!("abandoned-{saved}"), "inputs":[], "deliverables":["patch"],
        "acceptance_criteria":["review"], "constraints":[], "workspace":{}, "report_policy":"attached"
    })).unwrap()).unwrap();
    match saved.as_str() {
        "Done" => assert_eq!(
            host.runtime.wait(lane.root_run_id).await.unwrap(),
            AgentRunPhase::Done
        ),
        "Error" => assert_eq!(
            host.runtime.wait(lane.root_run_id).await.unwrap(),
            AgentRunPhase::Error
        ),
        "Checkpoint" => phase(&host.runtime, lane.root_run_id, AgentRunPhase::Waiting).await,
        _ => panic!("unknown phase"),
    }
    assert_eq!(
        host.runtime
            .restore_diagnostics(lane.root_run_id)
            .unwrap()
            .unwrap()
            .checkpoint_phase,
        saved
    );
    assert_eq!(
        host.owner.attach(&lane.thread_id).unwrap().state,
        OwnerState::Running
    );
    println!(
        "ABANDONED {}",
        json!({"lead":lead.to_string(), "lane":lane.lane_id,
        "thread":lane.thread_id, "run":lane.root_run_id.to_string(), "owner":host.owner.owner_id()})
    );
    std::io::stdout().flush().unwrap();
    // Keep the real host alive without executing Drop, until the parent SIGKILLs it.
    std::future::pending::<()>().await;
}
async fn killed_lane(saved: &str) {
    let dir = test_socket_dir::SplitDir::new();
    std::fs::write(
        dir.path().join("config.toml"),
        "[ownership]\ngrace_ms = 86400000\n",
    )
    .unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "abandoned_subprocess_worker",
            "--nocapture",
        ])
        .env("ABANDONED_ROOT", dir.path())
        .env("ABANDONED_SOCKETS", dir.sockets())
        .env("ABANDONED_PHASE", saved)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let checkpoint: Value = loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("source exited before checkpoint");
        if let Some(json) = line.strip_prefix("ABANDONED ") {
            break serde_json::from_str(json).unwrap();
        }
    };
    child.start_kill().unwrap();
    assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
    let thread = checkpoint["thread"].as_str().unwrap();
    let lane = checkpoint["lane"].as_str().unwrap();
    let lead: RunId = checkpoint["lead"].as_str().unwrap().parse().unwrap();
    let run: RunId = checkpoint["run"].as_str().unwrap().parse().unwrap();
    let db = dir.sockets().join("owners.db");
    let mut registry = Registry::open(&db).unwrap();
    let killed = registry.attach(thread).unwrap();
    assert_eq!(killed.state, OwnerState::Running);
    assert_eq!(killed.lease.owner_id, checkpoint["owner"].as_str().unwrap());
    let socket = dir
        .sockets()
        .join(format!("{}.sock", killed.lease.owner_id));
    assert!(
        socket.exists(),
        "SIGKILL socket must not be removed by the fixture"
    );
    // Grace-period negative: move expiry just before the source lease began.
    // The configured day-long grace avoids making test runtime a correctness condition;
    // exact grace boundaries are checked separately with an explicit clock.
    let before = if saved == "Checkpoint" {
        registry
            .update(thread, |row| {
                row.lease.expires_at = killed.lease.expires_at.saturating_sub(5001);
                Ok(())
            })
            .unwrap()
    } else {
        expire(&mut registry, thread)
    };
    let model = Arc::new(Model::default());
    let mut host = open(dir.path(), dir.sockets(), model.clone()).await;
    // No committed manifest exists. Startup must NOT be the path that claims either row.
    assert_eq!(registry.attach(thread).unwrap(), before);
    assert_eq!(
        registry.attach("lead").unwrap().lease.owner_id,
        killed.lease.owner_id
    );
    assert!(host.runtime.inspect_agent(run).is_err());
    expire(&mut registry, "lead");
    continue_thread(&host, "lead", lead);
    phase(&host.runtime, lead, AgentRunPhase::Waiting).await;
    let writer = host.composition.storage_writer.clone();
    let record = writer.lane(lane).unwrap().unwrap();
    let request = json!({"operation_id":"supplement", "lane_id":lane,
        "expected_management_revision":record.management_revision,
        "expected_brief_revision":record.brief_revision, "message":TEXT});
    if saved == "Checkpoint" {
        // Set up a durable held instruction without scheduling an active receiver.
        writer
            .apply_lane_instruction(
                storage::lane_instructions::LaneInstructionRequest {
                    operation_id: "supplement".into(),
                    lane_id: lane.into(),
                    expected_management_revision: record.management_revision,
                    expected_brief_revision: record.brief_revision,
                    action: storage::lane_instructions::LaneInstructionAction::Supplement {
                        message: TEXT.into(),
                    },
                },
                storage::lane_instructions::LaneInstructionActor {
                    thread_id: "lead".into(),
                    root_run_id: lead.to_string(),
                },
                1,
            )
            .unwrap();
        assert_eq!(
            host.runtime.drive_lane_deliveries(thread).await.unwrap(),
            runtime::lane_delivery::LaneDeliveryOutcome::Nacked(
                storage::lane_delivery::LaneDeliveryNack::Readonly
            )
        );
        assert_eq!(registry.attach(thread).unwrap(), before);
        assert_eq!(
            writer.lane_instruction_delivery(lane, 1).unwrap(),
            Some(storage::lane_instructions::LaneInstructionDelivery::Queued)
        );
        assert!(
            !writer
                .lane_inbox(thread.into(), String::new())
                .unwrap()
                .is_empty()
        );
        expire(&mut registry, thread);
        continue_thread(&host, thread, run);
    } else {
        // Real model tool dispatch, not a direct claim or synthetic delivery host.
        model
            .scripted
            .lock()
            .unwrap()
            .insert(lead.to_string(), request);
        host.runtime
            .send_message(lead, "send the supplement".into())
            .unwrap();
    }
    loop {
        let notified = model.received.notified();
        if model
            .seen
            .lock()
            .unwrap()
            .get(&run.to_string())
            .is_some_and(|s| s.contains(TEXT))
        {
            break;
        }
        notified.await;
    }
    phase(&host.runtime, run, AgentRunPhase::Waiting).await;
    let claimed = registry.attach(thread).unwrap();
    assert_eq!(claimed.lease.owner_id, host.owner.owner_id());
    assert_eq!(claimed.lease.generation, killed.lease.generation + 1);
    let old = runtime::ownership::OwnerPermit {
        registry_path: db,
        thread_id: thread.into(),
        lease: killed.lease.clone(),
        run_id: None,
    };
    assert!(old.validate_generation().is_err());
    // Receiver snapshot and receipt commit precede observing the text.
    let notification = writer
        .lane_instruction(lane, 1)
        .unwrap()
        .unwrap()
        .notification_id;
    let storage_path = host.composition.storage.db_path.clone();
    assert_eq!(
        writer.lane_instruction_delivery(lane, 1).unwrap(),
        Some(storage::lane_instructions::LaneInstructionDelivery::Delivered)
    );
    let receipts = writer
        .host_input_receipts(&[format!("lane-notification:{notification}")])
        .unwrap();
    assert!(
        receipts
            .iter()
            .any(|r| r.status == storage::HostInputStatus::Delivered
                && matches!(
                    r.provenance,
                    storage::HostInputProvenance::LaneNotification { ref notification_id, ref destination_thread_id }
                    if notification_id == &notification && destination_thread_id == thread
                )),
        "durable receiver receipt"
    );
    // Shutdown drains producer operations; read durable ACK after the writer closes.
    host.shutdown().await.unwrap();
    let conn = rusqlite::Connection::open(storage_path).unwrap();
    let (state, receipt): (String, String) = conn.query_row(
        "SELECT n.state,r.status FROM lane_notifications n JOIN host_input_receipts r ON r.prompt_id=n.receipt_prompt_id WHERE n.notification_id=?1",
        [&notification], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(state, "acked");
    assert_eq!(receipt, "delivered");
    assert!(
        socket.exists(),
        "successor must not unlink predecessor socket"
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn sigkill_done_lane_is_claimed_and_receives_lane_send() {
    killed_lane("Done").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn sigkill_error_lane_is_claimed_and_receives_lane_send() {
    killed_lane("Error").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn sigkill_checkpoint_lane_continue_claims_and_delivers_pending_instruction() {
    killed_lane("Checkpoint").await;
}
