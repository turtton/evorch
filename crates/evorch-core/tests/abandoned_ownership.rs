//! SIGKILL leaves ordinary non-manifest snapshots and durable lane bindings.
//! A fresh core composition reuses them through Continue and lane_send, without
//! registry edits, owner claims, or grace waits. Synchronization uses IPC/events.
#![cfg(unix)]
use event_bus::{AgentRunPhase, EventKind, ToolEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
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

const TEXT: &str = "crash-reuse durable supplement";
#[derive(Default)]
struct Model {
    source: bool,
    source_lead: Mutex<Option<String>>,
    scripted: Mutex<HashMap<String, Value>>,
    seen: Mutex<HashMap<String, String>>,
    received: tokio::sync::Notify,
    supplement_calls: Mutex<HashMap<String, usize>>,
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
            *self
                .supplement_calls
                .lock()
                .unwrap()
                .entry(invocation.run_id.clone())
                .or_default() += 1;
        }
        self.received.notify_one();
        let script = self.scripted.lock().unwrap().remove(&invocation.run_id);
        // Both the lead and an independent lane root are Orchestrators. Identify
        // the fixture lane by run identity, never by text or role alone.
        let source_lane = self.source
            && self
                .source_lead
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|lead| lead != invocation.run_id);
        let tool = if let Some(input) = script {
            Some(("lane_send", input))
        } else if source_lane && body.contains("abandoned-Done") {
            Some(("finish", json!({"result":"complete"})))
        } else {
            None
        };
        if source_lane && body.contains("abandoned-Error") {
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
            assert!(
                !matches!(agent.phase, AgentRunPhase::Done | AgentRunPhase::Error),
                "unexpected terminal phase while awaiting {wanted:?}: {agent:?}"
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
fn assert_no_owner_artifacts(root: &Path) {
    assert!(!root.join("owners.db").exists());
    assert!(std::fs::read_dir(root).unwrap().all(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|extension| extension != "sock")
    }));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "private subprocess entry point"]
async fn abandoned_subprocess_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("ABANDONED_ROOT").unwrap());
    let sockets = std::path::PathBuf::from(std::env::var_os("ABANDONED_SOCKETS").unwrap());
    let saved = std::env::var("ABANDONED_PHASE").unwrap();
    let model = Arc::new(Model {
        source: true,
        ..Default::default()
    });
    let host = open(&root, &sockets, model.clone()).await;
    let lead = host
        .runtime
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "lead conversation".into(),
            RunConfig {
                project_root: Some(root.clone()),
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    *model.source_lead.lock().unwrap() = Some(lead.to_string());
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
    // Force the lead to observe the lane outcome before SIGKILL too. The fixture
    // must not interpret a lane marker in the lead history as a command to finish
    // or fail the lead; report delivery can otherwise race the checkpoint.
    let report = format!("lane outcome mentions abandoned-{saved}");
    host.runtime.send_message(lead, report.clone()).unwrap();
    loop {
        let notified = model.received.notified();
        if model
            .seen
            .lock()
            .unwrap()
            .get(&lead.to_string())
            .is_some_and(|body| body.contains(&report))
        {
            break;
        }
        notified.await;
    }
    phase(&host.runtime, lead, AgentRunPhase::Waiting).await;
    assert_eq!(
        host.runtime
            .restore_diagnostics(lane.root_run_id)
            .unwrap()
            .unwrap()
            .checkpoint_phase,
        saved
    );
    assert_no_owner_artifacts(&sockets);
    println!(
        "ABANDONED {}",
        json!({"lead":lead.to_string(), "lane":lane.lane_id,
        "thread":lane.thread_id, "run":lane.root_run_id.to_string()})
    );
    std::io::stdout().flush().unwrap();
    // Keep the real host alive without executing Drop, until the parent SIGKILLs it.
    std::future::pending::<()>().await;
}
async fn killed_lane(saved: &str) {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let mut sidebar = workspace_state::SidebarState::default();
    sidebar
        .add_project(
            workspace_state::ProjectId::new("fixture-project"),
            "fixture",
            &root,
        )
        .unwrap();
    sidebar.projects[0].trust = workspace_state::TrustState::Approved;
    std::fs::write(
        root.join("sidebar.json"),
        serde_json::to_vec(&sidebar).unwrap(),
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
    eprintln!("{saved}: source checkpoint {checkpoint}");
    child.start_kill().unwrap();
    assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
    let thread = checkpoint["thread"].as_str().unwrap();
    let lane = checkpoint["lane"].as_str().unwrap();
    let lead: RunId = checkpoint["lead"].as_str().unwrap().parse().unwrap();
    let run: RunId = checkpoint["run"].as_str().unwrap().parse().unwrap();
    assert_no_owner_artifacts(dir.sockets());
    let model = Arc::new(Model::default());
    let mut host = open(dir.path(), dir.sockets(), model.clone()).await;
    eprintln!("{saved}: successor opened");
    // Cold startup does not auto-resurrect ordinary, non-manifest runs.
    assert!(host.runtime.inspect_agent(run).is_err());
    assert!(host.runtime.inspect_agent(lead).is_err());
    assert_no_owner_artifacts(dir.sockets());
    continue_thread(&host, "lead", lead);
    phase(&host.runtime, lead, AgentRunPhase::Waiting).await;
    eprintln!("{saved}: lead continued");
    let writer = host.composition.storage_writer.clone();
    let record = writer.lane(lane).unwrap().unwrap();
    let request = json!({"operation_id":"supplement", "message":TEXT, "targets":[{
        "lane_id":lane,
        "expected_management_revision":record.management_revision,
        "expected_brief_revision":record.brief_revision}]});
    let mut delivery_events = host.runtime.event_bus().subscribe();
    if saved == "Checkpoint" {
        continue_thread(&host, thread, run);
        phase(&host.runtime, run, AgentRunPhase::Waiting).await;
    }
    {
        // Real model tool dispatch, not a direct claim or synthetic delivery host.
        model
            .scripted
            .lock()
            .unwrap()
            .insert(lead.to_string(), request);
        host.runtime
            .send_message(lead, "send the supplement".into())
            .unwrap();
        loop {
            let event = delivery_events.recv().await.unwrap();
            if let EventKind::Tool(ToolEvent::ToolCompleted {
                run_id,
                tool_name,
                is_error,
                output,
                ..
            }) = event.kind
                && run_id.as_deref() == Some(lead.to_string().as_str())
                && tool_name == "lane_send"
            {
                assert!(!is_error, "lane_send failed: {output:?}");
                let result: Value =
                    serde_json::from_str(&output.expect("lane_send output")).unwrap();
                let results = result["results"].as_array().expect("per-lane results");
                assert_eq!(results.len(), 1, "{result:?}");
                assert_eq!(results[0]["lane_id"], lane, "{result:?}");
                assert_eq!(results[0]["status"], "accepted", "{result:?}");
                break;
            }
        }
    }
    eprintln!("{saved}: delivery requested");
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
    eprintln!("{saved}: supplement observed");
    phase(&host.runtime, run, AgentRunPhase::Waiting).await;
    eprintln!("{saved}: receiver waiting");
    assert_no_owner_artifacts(dir.sockets());
    assert_eq!(
        model.seen.lock().unwrap()[&run.to_string()]
            .matches(TEXT)
            .count(),
        1,
        "the durable supplement is present exactly once in receiver input"
    );
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
    assert_no_owner_artifacts(dir.sockets());
    assert_eq!(
        model.supplement_calls.lock().unwrap().get(&run.to_string()),
        Some(&1),
        "one receiver request executes the supplement"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sigkill_done_lane_is_reused_and_receives_lane_send() {
    killed_lane("Done").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sigkill_error_lane_is_reused_and_receives_lane_send() {
    killed_lane("Error").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sigkill_checkpoint_lane_continue_reuses_thread_and_receives_lane_send() {
    killed_lane("Checkpoint").await;
}
