//! Real OS-process capture/kill/restore through the core-only production bootstrap.
//! IPC checkpoints and lifecycle events, never sleeps or test-local deadlines.
use event_bus::{AgentRunPhase, EventKind, LifecycleEvent, ProviderEvent};
use evorch_core::recovery_service::CoreGraphRecovery;
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use runtime::{
    Role, RunConfig, RunId,
    quiesce::{CORE_RECOVERY_SETTINGS_REFERENCE, RecoveryRecipe, ResumePolicy},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams,
    models::{ProductionModel, compose_production_model},
};

#[path = "renderer_support/without_artifact_tool.rs"]
mod without_artifact_tool;
use without_artifact_tool::WithoutArtifactTool;

struct WaitModel {
    inputs: std::sync::Mutex<BTreeMap<String, runtime::quiesce::ProviderInput>>,
    source: bool,
    delegate: bool,
    calls: std::sync::atomic::AtomicUsize,
    child: std::sync::Mutex<Option<RunId>>,
    entered: tokio::sync::Notify,
    resumed: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
impl WaitModel {
    fn new(mode: &str) -> Self {
        Self {
            inputs: Default::default(),
            source: mode.starts_with("capture"),
            delegate: mode.ends_with("delegate"),
            calls: Default::default(),
            child: Default::default(),
            entered: Default::default(),
            resumed: Default::default(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}
#[async_trait::async_trait]
impl runtime::AgentModel for WaitModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "startup-wait-model".into()
    }
    fn checkpoint_provider_input(&self, run_id: &str) -> Option<runtime::quiesce::ProviderInput> {
        self.inputs.lock().unwrap().get(run_id).cloned()
    }
    fn validate_checkpoint_compatibility(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        pins: &[runtime::RoutePinRecord],
        input: Option<&runtime::quiesce::ProviderInput>,
    ) -> Result<(), runtime::RuntimeError> {
        assert!(pins.is_empty());
        assert!(input.is_none_or(|input| input.request.model == "startup-wait-model"));
        Ok(())
    }
    fn adopt_recovery_checkpoint(
        &self,
        run_id: &str,
        _: &[runtime::RoutePinRecord],
        input: Option<&runtime::quiesce::ProviderInput>,
    ) {
        if let Some(input) = input {
            self.inputs
                .lock()
                .unwrap()
                .insert(run_id.into(), input.clone());
        }
    }
    fn requires_admission(&self) -> bool {
        false
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        role: Role,
    ) -> Result<(), runtime::RuntimeError> {
        let _ = role;
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.inputs.lock().unwrap().insert(
            invocation.run_id.clone(),
            runtime::quiesce::ProviderInput {
                profile: "startup-wait-model".into(),
                protocol: model::ApiProtocol::OpenAiCompletions,
                edit_tool: config::EditToolConfig::Edit,
                affinity_key: invocation.run_id.clone(),
                replay: None,
                request: providers::ChatRequest {
                    model: "startup-wait-model".into(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    tool_choice: None,
                    temperature: None,
                    max_tokens: None,
                    reasoning_effort: None,
                    service_tier: None,
                    output_schema: None,
                    observation: None,
                    responses_replay: None,
                },
            },
        );
        if self.source && role == Role::Worker {
            // A registered child holds a provider call, not admission. Once the
            // source requests quiesce, return an unexecuted tool boundary so the
            // child remains resumable and the parent keeps its durable wait.
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            return Ok(providers::ChatResponse {
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::ToolUse {
                        id: "child-read".into(),
                        name: "read".into(),
                        input: json!({"file_path":"child-input.txt"}),
                    }],
                },
                usage: Default::default(),
                usage_reported: true,
                finish_reason: providers::FinishReason::ToolUse,
            });
        }
        if role == Role::Orchestrator && self.source {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.delegate || call > 0 {
                let (name, arguments) = if self.delegate {
                    (
                        "delegate",
                        json!({"target":{"role":"worker"}, "prompt":"Wait for current authority"}),
                    )
                } else {
                    (
                        "wait",
                        json!({"run_id":self.child.lock().unwrap().unwrap().to_string()}),
                    )
                };
                return Ok(providers::ChatResponse {
                    message: providers::Message {
                        role: providers::Role::Assistant,
                        content: vec![providers::ContentBlock::ToolUse {
                            id: "durable-wait-call".into(),
                            name: name.into(),
                            input: arguments,
                        }],
                    },
                    usage: Default::default(),
                    usage_reported: true,
                    finish_reason: providers::FinishReason::ToolUse,
                });
            }
        }
        if !self.source && role == Role::Orchestrator {
            let blocks = messages.iter().flat_map(|m| &m.content).collect::<Vec<_>>();
            assert_eq!(blocks.iter().filter(|b| matches!(b, providers::ContentBlock::ToolUse { id, .. } if id == "durable-wait-call")).count(), 1);
            assert_eq!(blocks.iter().filter(|b| matches!(b, providers::ContentBlock::ToolResult { tool_call_id, is_error:false, .. } if tool_call_id == "durable-wait-call")).count(), 1);
            self.resumed.notify_one();
        }
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "Waiting for the next user turn".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
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
fn emit(value: Value) {
    println!("06C {}", value);
    std::io::stdout().flush().unwrap();
}
async fn waiting(runtime: &runtime::AgentRuntime, run: RunId) {
    let mut events = runtime.event_bus().subscribe();
    loop {
        if runtime
            .inspect_agent(run)
            .is_ok_and(|r| r.phase == AgentRunPhase::Waiting)
        {
            return;
        }
        let event = events.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Error,
            reason,
            ..
        }) = event.kind
        {
            assert_ne!(run_id, run.to_string(), "recovery failed: {reason:?}");
        }
    }
}
async fn turn(events: &mut event_bus::EventReceiver, run: RunId) {
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. })
                if run_id == run.to_string() =>
            {
                return;
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error,
                reason,
                ..
            }) if run_id == run.to_string() => panic!("turn failed: {reason:?}"),
            _ => (),
        }
    }
}

fn ordinary_send(host: &BootstrappedHost, run: RunId, text: &str) {
    use workbench_host::commands::{ChatInput, ComposerRole, HostCommand, HostEvent};
    let mut commands = host.host.lock().unwrap();
    let events = commands.submit_local(
        HostCommand::SendChat(ChatInput {
            submission_id: uuid::Uuid::new_v4().to_string(),
            project_root: None,
            thread_id: "thread".into(),
            composer_role: ComposerRole::Orchestrator,
            text: text.into(),
            attachments: vec![],
            skills: vec![],
            model_preference: None,
            fork_seed: None,
        }),
        &workbench_protocol::envelope::Provenance::human(
            workbench_protocol::envelope::Surface::Cli,
            workbench_protocol::ids::ClientId::new("recovered-client").unwrap(),
        ),
    );
    assert!(
        matches!(events.as_slice(), [HostEvent::ChatAccepted { run_id, .. }] if run_id == &run.to_string()),
        "{events:?}"
    );
}

struct ProbeHook(String);
impl workbench_host::bootstrap::core_startup::CoreStartupHook for ProbeHook {
    fn start<'a>(
        &'a self,
        mut inputs: workbench_host::bootstrap::core_startup::CoreStartupInputs,
        services: &'a mut workbench_host::bootstrap::services::BackgroundServices,
    ) -> workbench_host::bootstrap::core_startup::CoreStartupFuture<'a> {
        use runtime::RecoveryActivationBoundary as Boundary;
        if !self.0.starts_with("cold") && self.0 != "observe" {
            if self.0 == "restore_twice" {
                inputs
                    .runtime
                    .install_planned_recovery_fixture_with_metadata(
                        "fixture-before-artifact",
                        "fixture-build",
                        inputs
                            .runtime
                            .restart_builtin_tool_names()
                            .into_iter()
                            .filter(|name| name != "inspect_artifact")
                            .collect(),
                    )
                    .unwrap();
            } else {
                inputs.runtime.install_planned_recovery_fixture().unwrap();
            }
            // This fixture exercises post-Ready graph recovery, not physical
            // bootstrap. Its explicit mode must agree with its injected proof;
            // planned_bootstrap.rs covers the real pre-Adopt/claim pipeline.
            if inputs.runtime.has_planned_restart_handoff() {
                inputs.mode = workbench_host::bootstrap::core_startup::CoreBootMode::PlannedHandoff;
            }
        }
        if self.0 == "missing_tool_evidence" {
            let db = rusqlite::Connection::open(&inputs.storage.db_path).unwrap();
            assert!(db.execute("DELETE FROM restart_run_tools", []).unwrap() > 0);
        }
        let target = match self.0.as_str() {
            "prepared" => Some(Boundary::Prepared),
            "before_ack" => Some(Boundary::BeforeFullAck),
            "after_ack" => Some(Boundary::AfterFullAck),
            _ => None,
        };
        if let Some(target) = target {
            let owner = inputs.owner.clone();
            inputs.runtime.set_recovery_activation_probe(move |boundary| {
                if boundary == target {
                    if boundary == Boundary::Prepared {
                        assert!(!owner.begin_quiesce().unwrap());
                        assert!(!owner.release_ready().unwrap());
                    }
                    emit(json!({"phase":"crash_boundary", "boundary":format!("{boundary:?}"), "pid":std::process::id()}));
                    // The parent kills this exact OS process after seeing the
                    // checkpoint. No time-based completion or global fault env.
                    loop { std::thread::park(); }
                }
                Ok(())
            }).unwrap();
        }
        if self.0 == "lost_ack" {
            inputs
                .runtime
                .set_recovery_activation_probe(|boundary| {
                    if boundary == Boundary::AfterFullAck {
                        return Err(runtime::RuntimeError::Model {
                            reason: "test: committed activation ACK lost".into(),
                        });
                    }
                    Ok(())
                })
                .unwrap();
        }
        workbench_host::bootstrap::core_startup::CoreStartupHook::start(
            &CoreGraphRecovery,
            inputs,
            services,
        )
    }
}

async fn open(
    root: &Path,
    sockets: &Path,
    mode: &str,
) -> (
    BootstrappedHost,
    event_bus::EventReceiver,
    Option<Arc<WaitModel>>,
) {
    let options = BootstrapOptions {
        config_dir: root.into(),
        ownership_root: sockets.to_path_buf(),
        project_root: root.into(),
    };
    let load_options = config::LoadOptions {
        user_config_dir: Some(root.into()),
        project_dir: Some(root.into()),
        read_env: false,
        ..Default::default()
    };
    let config = config::Config::load_strict(&load_options).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let events = bus.subscribe();
    let model = compose_production_model(
        &config,
        &ProductionModel {
            load_options,
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(root.join("fixture-credentials"))
                    .unwrap(),
            ),
            bus,
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_STARTUP_TEST_KEY",
                "offline-test-key",
            )])),
            model_catalog_dir: Some(root.join("catalog")),
        },
    )
    .unwrap();
    // A core from before the artifact provider sent no `inspect_artifact`.
    let model: Arc<dyn runtime::AgentModel> = if mode.ends_with("_old") {
        Arc::new(WithoutArtifactTool(
            model,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        ))
    } else {
        model
    };
    let waiting_model = (mode.ends_with("_wait") || mode.ends_with("_delegate"))
        .then(|| Arc::new(WaitModel::new(mode)));
    let model: Arc<dyn runtime::AgentModel> = waiting_model
        .as_ref()
        .map_or(model as Arc<dyn runtime::AgentModel>, |m| m.clone());
    let host = BootstrappedHost::open_for_core_with_test_seams(
        options,
        TestSeams {
            model,
            sandbox: Arc::new(Direct),
        },
        &ProbeHook(mode.into()),
    )
    .await
    .unwrap();
    (host, events, waiting_model)
}
fn recipe() -> RecoveryRecipe {
    RecoveryRecipe {
        settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    }
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "subprocess entry point; invoked with explicit private fixture arguments"]
async fn startup_subprocess_worker() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .try_init();
    let root = PathBuf::from(std::env::var_os("EVORCH_STARTUP_ROOT").unwrap());
    let sockets = PathBuf::from(std::env::var_os("EVORCH_STARTUP_SOCKETS").unwrap());
    let mode = std::env::var("EVORCH_STARTUP_MODE").unwrap();
    let (mut host, mut provider_events, waiting_model) = open(&root, &sockets, &mode).await;
    if mode.starts_with("capture") {
        let permit = host.owner.start("thread").unwrap();
        let run = host
            .runtime
            .delegate_chat(
                "thread",
                Role::Orchestrator,
                "Preserve the already-sent provider prefix".into(),
                RunConfig {
                    ownership: Some(permit),
                    project_root: Some(root.clone()),
                    memory: workbench_host::lane::memory_boundary(
                        &host.runtime,
                        Some(&host.composition.storage),
                        Some(&root),
                    ),
                    interactive: true,
                    conversation: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        if let Some(model) = &waiting_model {
            if !model.delegate {
                waiting(&host.runtime, run).await;
                let child = host
                    .runtime
                    .delegate_background_as_child(
                        run,
                        Role::Worker,
                        "Registered child waits for capture",
                        RunConfig {
                            project_root: Some(root.clone()),
                            ownership: Some(host.owner.owned_permit("thread").unwrap()),
                            ..RunConfig::default()
                        },
                    )
                    .unwrap();
                *model.child.lock().unwrap() = Some(child);
                model.entered.notified().await;
                let mut events = host.runtime.event_bus().subscribe();
                host.runtime
                    .send_message(run, "Wait for the child".into())
                    .unwrap();
                loop {
                    if let EventKind::Tool(event_bus::ToolEvent::ToolStarted { tool_name, .. }) =
                        events.recv().await.unwrap().kind
                        && tool_name == "wait"
                    {
                        break;
                    }
                }
            } else {
                model.entered.notified().await;
            }
        } else {
            waiting(&host.runtime, run).await;
        }
        if mode == "capture_question" {
            let question = host
                .runtime
                .request_user_question(run, "Choose the restart scope".into(), vec![], true)
                .unwrap();
            std::fs::write(root.join("captured-question"), question.id).unwrap();
        }
        let store = host.composition.storage_writer.clone();
        store
            .upsert_thread_runtime(&storage::threads::ThreadUpsert {
                thread_id: "thread".into(),
                project_root: Some(root.to_string_lossy().into_owned()),
                root_run_id: Some(run.to_string()),
                chat_role: Some(Role::Orchestrator.name().into()),
                ..Default::default()
            })
            .unwrap();
        host.services.request_stop();
        host.services.join_named("graph-recovery").await.unwrap();
        host.services.join_named("lane-recovery").await.unwrap();
        host.runtime.request_quiesce();
        if let Some(model) = &waiting_model {
            model.release.add_permits(1);
        }
        host.runtime.wait_quiesced().await.unwrap();
        let fence = host.runtime.fence_dispatch_restart().await.unwrap();
        let manifests = workbench_host::bootstrap::recovery::capture(
            &host.runtime,
            Some(&fence),
            "os-process-capture",
            &BTreeMap::from([(run, recipe())]),
        )
        .await
        .unwrap();
        std::fs::write(root.join("captured-run"), run.to_string()).unwrap();
        host.owner.begin_quiesce().unwrap();
        // An unfinished wait may still own an active turn. A real crash leaves
        // that lease intact; process B must use managed expiry retry, never steal it.
        host.owner.release_ready().unwrap();
        let lane_owner = if mode == "capture_lane" || mode == "capture_catalog_lane" {
            let orphan = host.runtime.reserve_run_id();
            store.create_lane(storage::lanes::NewLane {
                lane_id:"orphan".into(), thread_id:"orphan-thread".into(), origin_thread_id:"thread".into(), origin_run_id:run.to_string(), creation_key:"before-crash".into(),
                brief:serde_json::from_value(json!({"objective":"Recover the unowned seed once", "inputs":[], "deliverables":["result"], "acceptance_criteria":["observed"], "constraints":[], "workspace":{}, "report_policy":"attached"})).unwrap(),
                start_seed:json!({"run_id":orphan, "project_root":root, "requires_ownership":false}).to_string(),
                project_id:None, project_root:Some(root.to_string_lossy().into_owned()), title:None, at_ns:1,
            }).unwrap();
            std::fs::write(root.join("orphan-run"), orphan.to_string()).unwrap();
            let owner = runtime::ownership::OwnerHost::open(
                &sockets,
                Default::default(),
                Arc::new(event_bus::EventBus::new(64)),
            )
            .unwrap();
            if mode == "capture_lane" {
                owner.start("orphan-thread").unwrap();
                Some(owner)
            } else {
                None
            }
        } else {
            None
        };
        emit(
            json!({"phase":"captured", "pid":std::process::id(), "run":run, "manifests":manifests}),
        );
        if let Some(owner) = lane_owner {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "handoff");
            assert!(!owner.begin_quiesce().unwrap());
            assert!(!owner.release_ready().unwrap());
            emit(json!({"phase":"handed_off", "pid":std::process::id()}));
            std::future::pending::<()>().await;
        }
        std::future::pending::<()>().await;
    } else {
        if mode == "empty" {
            let report = host.recovery_diagnostics.as_ref().unwrap().snapshot();
            assert_eq!(report["components"], json!([]));
            emit(json!({"phase":"empty_bootstrapped", "pid":std::process::id(), "report":report}));
            host.shutdown().await.unwrap();
            return;
        }
        let run: RunId = std::fs::read_to_string(root.join("captured-run"))
            .unwrap()
            .parse()
            .unwrap();
        let report = host.recovery_diagnostics.as_ref().unwrap().snapshot();
        if host
            .recovery_diagnostics
            .as_ref()
            .unwrap()
            .has_pending_work()
        {
            assert!(
                host.busy(),
                "owner retry must survive core idle grace without local runs"
            );
        }
        emit(json!({"phase":"bootstrapped", "pid":std::process::id(), "report":report}));
        if mode == "restore_twice" {
            assert_eq!(report["components"][0]["state"], "activated", "{report}");
        }
        if mode == "catalog_failure" {
            assert_eq!(report["components"][0]["state"], "quarantined");
            assert_eq!(report["components"][0]["reason"], "needs_attention");
            assert_eq!(report["components"][0]["manual_continue"], false);
            assert!(host.runtime.inspect_agent(run).is_err());
            assert!(!host.runtime.has_paused_recovery());
            let orphan: RunId = std::fs::read_to_string(root.join("orphan-run"))
                .unwrap()
                .parse()
                .unwrap();
            waiting(&host.runtime, orphan).await;
            emit(
                json!({"phase":"unrelated_lane_restored", "pid":std::process::id(), "run":orphan}),
            );
            host.shutdown().await.unwrap();
            return;
        }
        if mode == "lost_ack" {
            assert!(
                !host
                    .host
                    .lock()
                    .unwrap()
                    .service()
                    .root_recovery_pending("thread")
            );
            assert!(
                host.host
                    .lock()
                    .unwrap()
                    .service()
                    .bindings
                    .root_of("thread")
                    .is_none()
            );
            assert_eq!(report["components"][0]["reason"], "already_activated");
            assert_eq!(report["components"][0]["manual_continue"], true);
            assert!(!host.runtime.has_paused_recovery());
            assert!(host.runtime.inspect_agent(run).is_err());
            let db = storage::Database::open(&host.composition.storage).unwrap();
            let claim = db.recovery_claims().unwrap().remove(0);
            assert_eq!(
                claim.state,
                storage::recovery::RecoveryClaimState::Activated
            );
            let before = db.recovery_claim_snapshot(&claim.claim_id).unwrap();
            // Wait for the parent to assert zero automatic provider requests.
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "continue");
            assert_eq!(db.recovery_claim_snapshot(&claim.claim_id).unwrap(), before);
            let permit = host.owner.owned_permit("thread").unwrap();
            host.runtime
                .continue_goal(
                    run,
                    "Explicit human Continue after ACK loss".into(),
                    RunConfig {
                        ownership: Some(permit),
                        project_root: Some(root.clone()),
                        interactive: true,
                        conversation: true,
                        keep_alive: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            waiting(&host.runtime, run).await;
            assert_eq!(db.recovery_claims().unwrap()[0], claim);
            emit(json!({"phase":"manual_continued", "pid":std::process::id()}));
            host.shutdown().await.unwrap();
            assert_eq!(db.recovery_claims().unwrap()[0], claim);
            return;
        }
        if mode == "missing_tool_evidence" || mode == "same_build_tools_mismatch" {
            assert_eq!(report["components"][0]["reason"], "needs_attention");
            assert_ne!(report["components"][0]["state"], "activated");
            assert!(
                !host
                    .host
                    .lock()
                    .unwrap()
                    .service()
                    .root_recovery_pending("thread")
            );
        }
        if mode == "observe"
            || mode.starts_with("cold")
            || mode == "missing_tool_evidence"
            || mode == "same_build_tools_mismatch"
        {
            assert!(
                host.host
                    .lock()
                    .unwrap()
                    .service()
                    .bindings
                    .root_of("thread")
                    .is_none()
            );
            assert!(
                host.host
                    .lock()
                    .unwrap()
                    .service()
                    .lane_bindings()
                    .get("thread")
                    .is_none()
            );
            host.shutdown().await.unwrap();
            return;
        }
        if mode == "restore_lane" {
            let orphan: RunId = std::fs::read_to_string(root.join("orphan-run"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                host.runtime.inspect_agent(orphan).is_err(),
                "live external owner must block initial lane admission"
            );
            emit(json!({"phase":"owner_blocked", "pid":std::process::id()}));
            waiting(&host.runtime, orphan).await;
            let permit = host.owner.owned_permit("orphan-thread").unwrap();
            assert_eq!(permit.lease.owner_id, host.owner.owner_id());
            emit(json!({"phase":"lane_restored", "pid":std::process::id(), "run":orphan}));
            host.shutdown().await.unwrap();
            return;
        }
        waiting(&host.runtime, run).await;
        if let Some(model) = &waiting_model {
            let db = storage::Database::open(&host.composition.storage).unwrap();
            let child: RunId = db
                .recovery_manifests()
                .unwrap()
                .iter()
                .flat_map(|m| &m.record.as_ref().unwrap().members)
                .find(|m| m.run_id != run.to_string())
                .unwrap()
                .run_id
                .parse()
                .unwrap();
            assert_eq!(host.runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
            // No new user message may interrupt the restored wait. The model
            // acknowledges consuming exactly one original successful tool result.
            model.resumed.notified().await;
            emit(
                json!({"phase":"restored", "pid":std::process::id(), "run":run, "cache_read_tokens":0}),
            );
            host.shutdown().await.unwrap();
            return;
        }
        let mut events = host.runtime.event_bus().subscribe();
        if mode == "restore_question" {
            let question = std::fs::read_to_string(root.join("captured-question")).unwrap();
            let saved = host.runtime.user_question(&question).unwrap().unwrap();
            assert_eq!(saved.run_id, run.to_string());
            assert!(saved.answer.is_none());
            host.runtime
                .answer_user_question(&question, "Use the captured scope")
                .unwrap();
        }
        if mode != "restore_question" {
            ordinary_send(&host, run, "Continue after the real process restart");
        }
        turn(&mut events, run).await;
        if mode == "restore_twice" {
            ordinary_send(&host, run, "And once more after the boundary");
            turn(&mut events, run).await;
        }
        let cached = if waiting_model.is_some() {
            0
        } else {
            loop {
                if let EventKind::Provider(ProviderEvent::RequestCompleted {
                    cache_read_tokens,
                    ..
                }) = provider_events.recv().await.unwrap().kind
                {
                    break cache_read_tokens;
                }
            }
        };
        emit(
            json!({"phase":"restored", "pid":std::process::id(), "run":run, "cache_read_tokens":cached}),
        );
        host.shutdown().await.unwrap();
    }
}
struct Process {
    child: tokio::process::Child,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
}
impl Process {
    fn spawn(dir: &test_socket_dir::SplitDir, mode: &str) -> Self {
        Self::spawn_with_proxy(dir, mode, None)
    }
    fn spawn_with_proxy(dir: &test_socket_dir::SplitDir, mode: &str, proxy: Option<&str>) -> Self {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        if let Some(proxy) = proxy {
            // Child-local proxy rejects models.dev CONNECT, never contacting the
            // external service. Provider requests stay on our loopback mock.
            command
                .env("HTTPS_PROXY", proxy)
                .env("https_proxy", proxy)
                .env("NO_PROXY", "127.0.0.1,localhost")
                .env("no_proxy", "127.0.0.1,localhost");
        }
        let mut child = command
            .args([
                "--ignored",
                "--exact",
                "startup_subprocess_worker",
                "--nocapture",
            ])
            .env("EVORCH_STARTUP_ROOT", dir.path())
            .env("EVORCH_STARTUP_SOCKETS", dir.sockets())
            .env("EVORCH_STARTUP_MODE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self { child, lines }
    }
    async fn checkpoint(&mut self, expected: &str) -> Value {
        while let Some(line) = self.lines.next_line().await.unwrap() {
            if let Some(value) = line.strip_prefix("06C ") {
                let value: Value = serde_json::from_str(value).unwrap();
                assert_eq!(value["phase"], expected, "unexpected subprocess checkpoint");
                assert_ne!(value["pid"].as_u64().unwrap(), std::process::id() as u64);
                eprintln!("startup subprocess reached {expected}: {value}");
                return value;
            }
        }
        panic!(
            "child exited before {expected}: {:?}",
            self.child.wait().await
        );
    }
    async fn kill(mut self) {
        self.child.start_kill().unwrap();
        assert!(!self.child.wait().await.unwrap().success());
    }
}
fn fixture(mock: &StreamingMockOpenAi) -> test_socket_dir::SplitDir {
    let dir = test_socket_dir::SplitDir::new();
    std::fs::write(dir.path().join("child-input.txt"), "durable child input").unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_STARTUP_TEST_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("catalog")).unwrap();
    std::fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        dir.path().join("catalog/models-dev.json"),
    )
    .unwrap();
    dir
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_missing_predecessor_tool_evidence_never_activates_or_publishes_bindings() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![ScriptedResponse::text_stream(
        "before",
        "mock-model",
        ["Waiting"],
    )]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture");
    source.checkpoint("captured").await;
    source.kill().await;
    let mut successor = Process::spawn(&dir, "missing_tool_evidence");
    let report = successor.checkpoint("bootstrapped").await;
    assert_eq!(
        report["report"]["components"][0]["reason"],
        "needs_attention"
    );
    assert!(successor.child.wait().await.unwrap().success());
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_same_build_tools_difference_is_needs_attention_without_provider_admission() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![ScriptedResponse::text_stream(
        "before",
        "mock-model",
        ["Waiting"],
    )]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture_old");
    source.checkpoint("captured").await;
    source.kill().await;
    let mut successor = Process::spawn(&dir, "same_build_tools_mismatch");
    let report = successor.checkpoint("bootstrapped").await;
    assert_eq!(
        report["report"]["components"][0]["reason"],
        "needs_attention"
    );
    assert!(successor.child.wait().await.unwrap().success());
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_owner_blocked_retry_binds_the_captured_root_before_ordinary_send() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting"]),
        ScriptedResponse::text_stream("after", "mock-model", ["Same recovered conversation"]),
    ]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture");
    let captured = source.checkpoint("captured").await;
    source.kill().await;
    // Capture has already released its idle owner. A distinct live owner claims
    // that generation before successor startup; its explicit quiesce/release is
    // the semantic retry trigger, never a sleep or an expiry assumption.
    let blocker = runtime::ownership::OwnerHost::open(
        dir.sockets(),
        Default::default(),
        Arc::new(event_bus::EventBus::new(64)),
    )
    .unwrap();
    blocker.claim(&blocker.attach("thread").unwrap()).unwrap();
    let mut successor = Process::spawn(&dir, "restore");
    let ready = successor.checkpoint("bootstrapped").await;
    assert_eq!(ready["report"]["components"][0]["state"], "blocked");
    assert_eq!(ready["report"]["components"][0]["reason"], "owner_blocked");
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
    assert!(!blocker.begin_quiesce().unwrap());
    assert!(!blocker.release_ready().unwrap());
    let continued = successor.checkpoint("restored").await;
    assert_eq!(continued["run"], captured["run"]);
    assert!(successor.child.wait().await.unwrap().success());
    let requests = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &requests[1]).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_process_cache_preserves_wire_prefix_after_planned_adoption() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting for the user"]),
        ScriptedResponse::text_stream("after", "mock-model", ["Restored in another process"]),
    ]);
    let dir = fixture(&mock);
    let mut first = Process::spawn(&dir, "capture");
    let captured = first.checkpoint("captured").await;
    first.kill().await;
    let mut second = Process::spawn(&dir, "restore");
    let bootstrapped = second.checkpoint("bootstrapped").await;
    assert_ne!(captured["pid"], bootstrapped["pid"]);
    assert_eq!(
        bootstrapped["report"]["components"][0]["state"],
        "activated"
    );
    let restored = second.checkpoint("restored").await;
    assert_eq!(restored["run"], captured["run"]);
    assert!(restored["cache_read_tokens"].as_u64().unwrap() > 0);
    assert!(second.child.wait().await.unwrap().success());
    // The logical planned seam has already published Ready before graph startup.
    let config = storage::StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..Default::default()
    };
    let transaction = storage::Database::open(&config)
        .unwrap()
        .restart_transactions()
        .unwrap()
        .remove(0);
    assert!(transaction.ready_at_ms.is_some());
    assert_eq!(
        transaction.phase,
        storage::recovery::restart::RestartPhase::Completed
    );
    let mut cold = Process::spawn(&dir, "cold_observe");
    let observed = cold.checkpoint("bootstrapped").await;
    assert!(
        observed["report"]["components"]
            .as_array()
            .unwrap()
            .is_empty(),
        "Completed is not a repeated cold-start alert"
    );
    assert!(cold.child.wait().await.unwrap().success());
    let requests = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    // The run restored by startup recovery sees the artifact provider's catalog
    // exactly as before the restart: `inspect_artifact` once, same array.
    for request in &requests {
        let names = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["function"]["name"] == "inspect_artifact")
            .count();
        assert_eq!(names, 1, "inspect_artifact is offered once");
    }
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &requests[1]).unwrap_or_else(|error| {
        let changed = requests[0]
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| {
                key.as_str() != "messages" && requests[1].get(*key) != requests[0].get(*key)
            })
            .cloned()
            .collect::<Vec<_>>();
        panic!("{error}; changed request fields: {changed:?}");
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_recovery_of_a_conversation_from_before_the_artifact_tool_gets_one_tools_only_boundary()
 {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting for the user"]),
        ScriptedResponse::text_stream("after", "mock-model", ["Restored in another process"]),
        ScriptedResponse::text_stream("again", "mock-model", ["The next turn"]),
    ]);
    let dir = fixture(&mock);
    let mut first = Process::spawn(&dir, "capture_old");
    first.checkpoint("captured").await;
    first.kill().await;
    let mut second = Process::spawn(&dir, "restore_twice");
    second.checkpoint("bootstrapped").await;
    second.checkpoint("restored").await;
    assert!(second.child.wait().await.unwrap().success());
    let requests = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);
    let names = |request: &Value| -> Vec<String> {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let offers = |request: &Value| {
        names(request)
            .iter()
            .filter(|name| name.ends_with("inspect_artifact"))
            .count()
    };
    assert_eq!(
        offers(&requests[0]),
        0,
        "the old core sent no artifact tool"
    );
    // The boundary: the resumed run sees the current catalog, which differs
    // from the stored one by `inspect_artifact` alone ...
    assert_eq!(offers(&requests[1]), 1);
    let before = names(&requests[0]);
    let after = names(&requests[1]);
    assert_eq!(after.len(), before.len() + 1);
    assert!(before.iter().all(|name| after.contains(name)));
    // ... and nothing else changed: with the old array put back, the request
    // is an ordinary append to the stored conversation.
    let mut as_before = requests[1].clone();
    as_before["tools"] = requests[0]["tools"].clone();
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &as_before).unwrap();
    // The turn after the boundary reuses the new prefix.
    assert_eq!(requests[1]["tools"], requests[2]["tools"]);
    assert_append_only(CacheProtocol::OpenAi, &requests[1], &requests[2]).unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_prepared_process_crash_quarantines_the_reserved_claim() {
    crash_boundary("prepared", "Prepared", false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_before_activation_ack_crash_quarantines_without_retry() {
    crash_boundary("before_ack", "BeforeFullAck", false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_after_activation_ack_crash_is_completed_without_replay() {
    crash_boundary("after_ack", "AfterFullAck", true).await;
}
async fn crash_boundary(mode: &str, boundary: &str, activated: bool) {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting"]),
        ScriptedResponse::text_stream("after", "mock-model", ["Restored"]),
    ]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture");
    source.checkpoint("captured").await;
    source.kill().await;
    let mut crashing = Process::spawn(&dir, mode);
    let checkpoint = crashing.checkpoint("crash_boundary").await;
    assert_eq!(checkpoint["boundary"], boundary);
    let config = storage::StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..Default::default()
    };
    let claim = storage::Database::open(&config)
        .unwrap()
        .recovery_claims()
        .unwrap()
        .remove(0);
    assert_eq!(
        claim.state,
        if activated {
            storage::recovery::RecoveryClaimState::Activated
        } else {
            storage::recovery::RecoveryClaimState::Prepared
        }
    );
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1,
        "no provider work before publication"
    );
    crashing.kill().await;
    let mut recovered = Process::spawn(&dir, "cold_observe");
    let ready = recovered.checkpoint("bootstrapped").await;
    assert_ne!(ready["pid"], checkpoint["pid"]);
    if activated {
        let tx = storage::Database::open(&config)
            .unwrap()
            .restart_transactions()
            .unwrap()
            .remove(0);
        assert!(tx.ready_at_ms.is_some());
        assert_eq!(
            tx.phase,
            storage::recovery::restart::RestartPhase::Completed
        );
        assert!(
            ready["report"]["components"].as_array().unwrap().is_empty(),
            "Ready plus FULL ACK completed the transaction; cold must not replay it"
        );
    } else {
        assert_eq!(ready["report"]["components"][0]["state"], "quarantined");
        assert_eq!(
            ready["report"]["components"][0]["reason"],
            "needs_attention"
        );
    }
    assert!(recovered.child.wait().await.unwrap().success());
    let after = storage::Database::open(&config)
        .unwrap()
        .recovery_claims()
        .unwrap()
        .remove(0);
    assert_eq!(after.claim_id, claim.claim_id);
    assert_eq!(
        after.members, claim.members,
        "adoption must not reserve another revision/incarnation"
    );
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_cold_unexecuted_and_corrupt_claims_quarantine_without_release_or_replay() {
    for corruption in ["settings", "claim", "restart"] {
        let mock =
            StreamingMockOpenAi::spawn_with_prompt_cache(vec![ScriptedResponse::text_stream(
                "before",
                "mock-model",
                ["Waiting"],
            )]);
        let dir = fixture(&mock);
        let mut source = Process::spawn(&dir, "capture");
        source.checkpoint("captured").await;
        source.kill().await;
        let mut prepared = Process::spawn(&dir, "prepared");
        prepared.checkpoint("crash_boundary").await;
        prepared.kill().await;
        let sql = rusqlite::Connection::open(dir.path().join("evorch-events.db")).unwrap();
        let intent = || {
            sql.query_row(
                "SELECT incarnation,revision,payload FROM resume_intents",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap()
        };
        let before = intent();
        if corruption == "claim" {
            sql.execute(
                "UPDATE recovery_claim_members SET old_incarnation=x'ff'",
                [],
            )
            .unwrap();
        } else if corruption == "restart" {
            sql.execute("UPDATE restart_expected_members SET member_json='{'", [])
                .unwrap();
        } else {
            // Emulate an unsupported settings selector in an otherwise readable
            // captured component. Do not hand-author a snapshot or a claim.
            sql.execute("UPDATE recovery_members SET record_json=json_set(record_json,'$.recipe.settings_reference','evorch/core/unsupported')", []).unwrap();
        }
        let mut observer = Process::spawn(&dir, "observe");
        let ready = observer.checkpoint("bootstrapped").await;
        let diagnostic = &ready["report"]["components"][0];
        assert_eq!(diagnostic["state"], "quarantined");
        assert_eq!(diagnostic["manual_continue"], false);
        assert!(observer.child.wait().await.unwrap().success());
        assert_eq!(
            intent(),
            before,
            "declining a claim must not rewrite the reserved intent"
        );
        let state: String = sql
            .query_row("SELECT state FROM recovery_claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "prepared", "cold boot cannot release the old claim");
        assert_eq!(
            mock.recorded_requests()
                .iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_wait_and_delegate_wait_restore_the_captured_tool_continuation() {
    for kind in ["wait", "delegate"] {
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![]);
        let dir = fixture(&mock);
        let mut source = Process::spawn(&dir, &format!("capture_{kind}"));
        source.checkpoint("captured").await;
        let db = storage::Database::open(&storage::StorageConfig {
            db_path: dir.path().join("evorch-events.db"),
            ..Default::default()
        })
        .unwrap();
        let loads = db.recovery_manifests().unwrap();
        let records = loads
            .iter()
            .flat_map(|load| &load.record.as_ref().unwrap().members)
            .map(|member| {
                serde_json::from_str::<runtime::quiesce::RunRecoveryRecord>(&member.record_json)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .any(|r| r.waits.iter().any(|w| w.continuation.tool_name == kind))
        );
        source.kill().await;
        let mut recovered = Process::spawn(&dir, &format!("restore_{kind}"));
        let ready = recovered.checkpoint("bootstrapped").await;
        assert!(matches!(
            ready["report"]["components"][0]["state"].as_str(),
            Some("activated" | "blocked")
        ));
        recovered.checkpoint("restored").await;
        assert!(recovered.child.wait().await.unwrap().success());
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_question_is_durable_across_capture_kill_and_new_process_answer() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting"]),
        ScriptedResponse::text_stream("after", "mock-model", ["Answered"]),
    ]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture_question");
    source.checkpoint("captured").await;
    source.kill().await;
    let mut recovered = Process::spawn(&dir, "restore_question");
    recovered.checkpoint("bootstrapped").await;
    let restored = recovered.checkpoint("restored").await;
    assert!(recovered.child.wait().await.unwrap().success());
    assert!(restored["cache_read_tokens"].as_u64().unwrap() > 0);
    let requests = mock.recorded_requests();
    let chat = requests
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(chat.len(), 2);
    assert_append_only(CacheProtocol::OpenAi, &chat[0].body, &chat[1].body).unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_orphan_unowned_seed_waits_for_external_owner_then_starts_once() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting"]),
        ScriptedResponse::text_stream("lane", "mock-model", ["Recovered lane"]),
    ]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture_lane");
    source.checkpoint("captured").await;
    let mut recovered = Process::spawn(&dir, "restore_lane");
    recovered.checkpoint("bootstrapped").await;
    recovered.checkpoint("owner_blocked").await;
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
    source
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"handoff\n")
        .await
        .unwrap();
    source.checkpoint("handed_off").await;
    source.kill().await;
    recovered.checkpoint("lane_restored").await;
    assert!(recovered.child.wait().await.unwrap().success());
    let db = storage::Database::open(&storage::StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        db.lane_by_thread("orphan-thread")
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        2
    );
    let mut observer = Process::spawn(&dir, "observe");
    observer.checkpoint("bootstrapped").await;
    assert!(observer.child.wait().await.unwrap().success());
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        2
    );
}

// Exercise the production error path, not a successful empty-catalog substitute.
async fn catalog_outage(fetch_fails: bool) {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting"]),
        ScriptedResponse::text_stream("lane", "mock-model", ["Unrelated lane recovered"]),
    ]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture_catalog_lane");
    source.checkpoint("captured").await;
    source.kill().await;
    if !fetch_fails {
        let mut prepared = Process::spawn(&dir, "prepared");
        prepared.checkpoint("crash_boundary").await;
        prepared.kill().await;
    }
    let storage = storage::StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..Default::default()
    };
    let db = storage::Database::open(&storage).unwrap();
    let claims = db.recovery_claims().unwrap();
    std::fs::remove_file(dir.path().join("catalog/models-dev.json")).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.path().join("catalog/models-dev.lock"))
        .unwrap();
    if !fetch_fails {
        lock.lock().unwrap();
    }
    let proxy = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&proxy)
        .await;
    let mut recovered = Process::spawn_with_proxy(&dir, "catalog_failure", Some(&proxy.uri()));
    recovered.checkpoint("bootstrapped").await;
    recovered.checkpoint("unrelated_lane_restored").await;
    assert!(recovered.child.wait().await.unwrap().success());
    assert_eq!(
        db.recovery_claims().unwrap(),
        claims,
        "outage must not release Prepared"
    );
    assert_eq!(
        db.lane_by_thread("orphan-thread")
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    // Exactly the source capture and unrelated lane: the graph made zero new requests.
    let requests = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert!(
        !String::from_utf8(serde_json::to_vec(&requests[1].body).unwrap())
            .unwrap()
            .contains("Preserve the already-sent provider prefix")
    );
    if fetch_fails {
        assert!(!proxy.received_requests().await.unwrap().is_empty());
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_missing_catalog_cache_holds_prepared_and_recovers_unrelated_lane() {
    catalog_outage(false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_catalog_fetch_failure_holds_graph_and_recovers_unrelated_lane() {
    catalog_outage(true).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_catalog_outage_without_graphs_does_not_prevent_core_boot() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![]);
    let dir = fixture(&mock);
    std::fs::remove_file(dir.path().join("catalog/models-dev.json")).unwrap();
    let proxy = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&proxy)
        .await;
    let mut empty = Process::spawn_with_proxy(&dir, "empty", Some(&proxy.uri()));
    empty.checkpoint("empty_bootstrapped").await;
    assert!(empty.child.wait().await.unwrap().success());
    assert!(mock.recorded_requests().is_empty());
    assert!(!proxy.received_requests().await.unwrap().is_empty());
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_same_process_ack_loss_cleans_only_for_explicit_manual_continue() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("before", "mock-model", ["Waiting"]),
        ScriptedResponse::text_stream("manual", "mock-model", ["Human Continue"]),
    ]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture");
    source.checkpoint("captured").await;
    source.kill().await;
    let mut recovered = Process::spawn(&dir, "lost_ack");
    recovered.checkpoint("bootstrapped").await;
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
    recovered
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"continue\n")
        .await
        .unwrap();
    recovered.checkpoint("manual_continued").await;
    assert!(recovered.child.wait().await.unwrap().success());
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_cold_captured_graph_never_adopts_or_calls_the_provider() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![ScriptedResponse::text_stream(
        "before",
        "mock-model",
        ["Waiting"],
    )]);
    let dir = fixture(&mock);
    let mut source = Process::spawn(&dir, "capture");
    source.checkpoint("captured").await;
    source.kill().await;
    let mut cold = Process::spawn(&dir, "cold_observe");
    let ready = cold.checkpoint("bootstrapped").await;
    assert!(
        ready["report"]["components"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["state"] == "quarantined")
    );
    assert!(cold.child.wait().await.unwrap().success());
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
    let config = storage::StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..Default::default()
    };
    assert!(
        storage::Database::open(&config)
            .unwrap()
            .recovery_claims()
            .unwrap()
            .is_empty()
    );
}
