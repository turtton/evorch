//! One public restart of the sealed product, driven only from outside: a
//! desktop-surface client starts an ordinary run, a CLI-surface client asks for
//! the restart over the public wire with the same typed calls the management CLI
//! uses, and the successor is observed through `/proc`, its lifetime lock and
//! its own wire. The predecessor is the product too (the artifact, no feature,
//! no test hook): this is a product-to-product restart on one thread of work.
//!
//! The provider is a mock that derives its cache accounting from the input it
//! receives. The run's first provider response is held at a gate so the run is
//! really in flight when the restart is asked for; the restart then has to wait
//! for the safe point instead of cutting the turn.
#![allow(dead_code)]
use super::product_exec::{
    artifact::ProductArtifact,
    env::IsolatedEnv,
    launch::{KEY_ENV, ProductLaunch, READY_WATCHDOG},
    observe,
    wire::MockProvider,
};
use mock_openai::{ResponseGate, ScriptedResponse};
use serde_json::Value;
use std::{path::Path, time::Duration};
use workbench_client::{
    ClientConfig, UnixTransport, WorkbenchClient,
    restart::{
        confirmation::confirm,
        intent::{TargetSelector, plan_restart, request_restart},
        session::RestartAdmin,
    },
};
use workbench_protocol::{
    commands::*,
    events::*,
    handshake::{BuildInfo, LocalSurface},
    ids::*,
    namespaces::{
        ConversationCommand, ExecutionEvent,
        lifecycle::{AdminId, AdminOperationState},
    },
    views::RunPhase,
};

/// Finite bound for every wait that ends on an event.
const EVENT_WATCHDOG: Duration = Duration::from_secs(120);

pub struct Evidence {
    pub pid_before: i32,
    pub pid_after: i32,
    pub exe_is_artifact_after: bool,
    pub epoch_before: u64,
    pub epoch_after: u64,
    pub run: String,
    /// Chat-completion request bodies in the order the provider received them.
    pub requests: Vec<Value>,
    /// The provider's own scripts still unused when the scenario ended.
    pub remaining_scripts: usize,
    /// The artifact's processes below the product, besides the product itself.
    pub extra_artifact_processes: usize,
    pub lifetime_free_while_running: bool,
    pub receipt_state: AdminOperationState,
}

/// The mock joins its workers on drop, so a held response must be let go on every exit path.
struct ReleaseOnDrop(ResponseGate);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

pub fn client_config(name: &str, surface: LocalSurface) -> ClientConfig {
    let mut config = ClientConfig::new(
        ClientId::new(name).unwrap(),
        BuildInfo {
            name: name.into(),
            version: "1".into(),
            revision: None,
        },
    );
    config.local_surface = Some(surface);
    config
}

pub async fn connect(socket: &Path, name: &str, surface: LocalSurface) -> WorkbenchClient {
    WorkbenchClient::connect(
        UnixTransport::connect(socket)
            .await
            .unwrap_or_else(|e| panic!("connect {}: {e}", socket.display())),
        client_config(name, surface),
    )
    .await
    .unwrap_or_else(|e| panic!("handshake as {name}: {e:?}"))
}

pub fn chat(submission: &str, text: &str) -> Command {
    Command::Conversation(ConversationCommand::SendChat(SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("restart-thread").unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Orchestrator,
        text: text.into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }))
}

/// Waits for the run's own event; resyncs are answered by a fresh snapshot.
pub async fn until_waiting(client: &WorkbenchClient, subscription: &mut Subscription, run: &str) {
    let mut resyncs = 0;
    let wait = async {
        while let Some(event) = subscription.next().await {
            if let UiEvent::Projection(
                workbench_protocol::namespaces::ProjectionEvent::ResyncRequired { reason },
            ) = &event.event
            {
                resyncs += 1;
                assert!(resyncs <= 3, "too many resyncs: {reason:?}");
                *subscription = client
                    .subscribe(Subscribe {
                        scope: SubscriptionScope::All,
                        after: None,
                    })
                    .await
                    .unwrap();
                if waiting_in_snapshot(subscription, run) {
                    return;
                }
                continue;
            }
            if let UiEvent::Execution(ExecutionEvent::RunChanged { run: summary }) = event.event
                && summary.id.as_str() == run
            {
                match summary.phase {
                    RunPhase::WaitingForUser => return,
                    RunPhase::Failed | RunPhase::Cancelled | RunPhase::Done => {
                        panic!("run ended: {:?}", summary.phase)
                    }
                    _ => (),
                }
            }
        }
        panic!("subscription ended: {:?}", subscription.end());
    };
    tokio::time::timeout(EVENT_WATCHDOG, wait)
        .await
        .expect("the run reached its next wait within the watchdog");
}

pub fn waiting_in_snapshot(subscription: &Subscription, run: &str) -> bool {
    subscription
        .started()
        .initial_page
        .as_ref()
        .is_some_and(|page| {
            page.items.iter().any(|item| {
                matches!(item, SnapshotItem::Run(summary)
                    if summary.id.as_str() == run && summary.phase == RunPhase::WaitingForUser)
            })
        })
}

pub use workbench_client::Subscription;

fn ready_line(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line)
        .ok()
        .filter(|v| v["status"] == "ready")
}

/// Starts the product, runs the whole scenario and takes the product back.

pub fn drive_with(artifact: &ProductArtifact, in_flight: bool) -> Evidence {
    let env = IsolatedEnv::new();
    let (gate, arrivals) = ResponseGate::new();
    let first = ScriptedResponse::text_stream("turn-1", "mock-model", ["first"]);
    let first_turn = if in_flight {
        first.with_gate(gate.clone())
    } else {
        first
    };
    let mut scripts = vec![first_turn];
    scripts.push(ScriptedResponse::text_stream(
        "turn-2",
        "mock-model",
        ["second"],
    ));
    scripts.push(ScriptedResponse::text_stream(
        "turn-3",
        "mock-model",
        ["third"],
    ));
    let provider = MockProvider::scripted(scripts);
    let _release = ReleaseOnDrop(gate.clone());
    env.write_product_config(&provider.base_url(), KEY_ENV);
    let mut product = ProductLaunch::new(artifact, &env)
        .env(
            "RUST_LOG",
            "info,evorch_core=debug,workbench_host=debug,core_launcher=debug",
        )
        .start()
        .unwrap();
    let ready = product.wait_ready().expect("the product reports Ready");
    let pid_before = product.pid();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let (run, epoch_before, receipt_state, keep_open) = runtime.block_on(async {
        let desktop = connect(&ready.socket, "scenario-desktop", LocalSurface::Desktop).await;
        let mut events = desktop
            .subscribe(Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .await
            .unwrap();
        let reply = desktop
            .command(chat("first", "start the work"))
            .await
            .expect("the core accepts the chat");
        let run = reply.run_id.expect("a run").as_str().to_owned();
        if in_flight {
            arrivals
                .recv_timeout(EVENT_WATCHDOG)
                .expect("the provider received the first request");
        } else {
            until_waiting(&desktop, &mut events, &run).await;
        }

        let admin_client = connect(&ready.socket, "scenario-cli", LocalSurface::Cli).await;
        let admin = RestartAdmin::new(&admin_client);
        assert!(
            admin.offers_restart(),
            "a verified local peer is offered safe restart"
        );
        let epoch_before = admin_client.server_hello().instance_epoch.0;
        let targets = admin.targets().await.unwrap();
        let operation = AdminId::new("scenario-restart-1").unwrap();
        let binding = plan_restart(&targets, &TargetSelector::Current, operation.clone()).unwrap();
        let proof = confirm(false, true, false).unwrap();
        let receipt = admin
            .send(request_restart(binding, proof))
            .await
            .expect("the request is accepted");
        assert_eq!(receipt.operation_id, operation);
        // The run is in flight: the restart is under way but still before capture,
        // waiting for the safe point instead of cutting the turn.
        let status = admin.status().await.unwrap();
        assert!(
            workbench_client::restart::intent::cancellable(status.status.phase),
            "restart phase {:?}",
            status.status.phase
        );
        gate.release();
        let state = receipt.state;
        (run, epoch_before, state, (admin_client, events, desktop))
    });

    // The successor reports Ready on the stdout the predecessor had.
    let (line, _) = product
        .process
        .wait_for(READY_WATCHDOG, ready_line)
        .unwrap_or_else(|error| {
            let status = runtime.block_on(async {
                let c = connect(&ready.socket, "scenario-diag", LocalSurface::Cli).await;
                let a = RestartAdmin::new(&c);
                format!(
                    "{:?}\noperation: {:?}",
                    a.status().await,
                    a.operation(&AdminId::new("scenario-restart-1").unwrap())
                        .await
                )
            });
            panic!(
                "the successor reports Ready: {error:?}\nstatus: {status}\nproduct stderr:\n{}",
                product.process.stderr()
            )
        });
    drop(keep_open);
    let socket = std::path::PathBuf::from(line["socket"].as_str().unwrap());
    assert_eq!(
        socket, ready.socket,
        "the successor serves the same endpoint"
    );
    let pid_after = product.pid();
    let exe_is_artifact_after =
        observe::same_file(Path::new(&format!("/proc/{pid_after}/exe")), &artifact.path);
    let extra_artifact_processes = observe::descendants(pid_after)
        .into_iter()
        .filter(|p| {
            p.exe
                .as_deref()
                .is_some_and(|exe| observe::same_file(exe, &artifact.path))
        })
        .count();
    let config = core_instance::ConfigIdentity::new(&env.config_dir()).unwrap();
    let lifetime_free_while_running = core_instance::LaunchGuard::acquire(&config)
        .and_then(|guard| guard.lifetime_available())
        .unwrap_or(false);

    let (epoch_after, requests_during) = runtime.block_on(async {
        let desktop = connect(&socket, "scenario-desktop-2", LocalSurface::Desktop).await;
        let epoch_after = desktop.server_hello().instance_epoch.0;
        let mut events = desktop
            .subscribe(Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .await
            .unwrap();
        assert!(
            waiting_in_snapshot(&events, &run),
            "the captured run is back, waiting for the user, in the successor's snapshot"
        );
        for (submission, text) in [("second", "continue"), ("third", "continue again")] {
            let reply = desktop
                .command(chat(submission, text))
                .await
                .expect("the successor accepts the follow-up");
            assert_eq!(
                reply.run_id.expect("a run").as_str(),
                run,
                "the follow-up continues the captured run, not a cold conversation"
            );
            until_waiting(&desktop, &mut events, &run).await;
        }
        (epoch_after, ())
    });
    let _ = requests_during;
    let requests: Vec<Value> = provider
        .chat_requests()
        .into_iter()
        .map(|r| r.body)
        .collect();
    let remaining_scripts = provider.remaining_scripts();
    let (report, footprint) = product.stop();
    assert!(report.success(), "{:?} {}", report.status, report.stderr);
    assert!(report.orphans.is_empty());
    drop(env);
    assert!(footprint.remains().is_empty());
    Evidence {
        pid_before,
        pid_after,
        exe_is_artifact_after,
        epoch_before,
        epoch_after,
        run,
        requests,
        remaining_scripts,
        extra_artifact_processes,
        lifetime_free_while_running,
        receipt_state,
    }
}
