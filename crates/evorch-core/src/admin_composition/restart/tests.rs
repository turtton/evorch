#![cfg(target_os = "linux")]
use crate::admin_composition::{Inputs, capability::Publication};
use crate::restart::tests::checked_target;
use workbench_protocol::{
    commands::Command,
    envelope::{
        Provenance, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope, Surface,
    },
    errors::ErrorCode,
    handshake::Capability,
    ids::{ClientId, RequestId, SessionId},
    namespaces::lifecycle::{LifecycleCommand, LifecycleQuery, LifecycleReply, restart::*},
    queries::{Query, QueryReply},
    wire::Revision,
};
use workbench_web::{WebSession, WorkbenchBackend};
#[path = "../../../shared-test-support/composed_core.rs"]
pub(super) mod composed;
use crate::admin_composition::recovery::support;
use composed::{ComposedCore, masked_child};
use support::{Conn, error_of, id, serve};

fn compose(core: &ComposedCore) -> Publication {
    crate::admin_composition::install(
        &Inputs::new(&core.host, &core.backend)
            .with_finalization(core.driver.finalization(), &core.context),
    )
}
fn query(name: &str, query: RestartQuery) -> RequestEnvelope {
    envelope(
        name,
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::Restart(query))),
    )
}
fn command(name: &str, command: RestartCommand) -> RequestEnvelope {
    envelope(
        name,
        RequestBody::Command(Command::Lifecycle(LifecycleCommand::Restart(command))),
    )
}
fn envelope(name: &str, body: RequestBody) -> RequestEnvelope {
    RequestEnvelope {
        request_id: RequestId::new(name).unwrap(),
        session_id: SessionId::new("session").unwrap(),
        body,
    }
}
fn view(reply: ReplyEnvelope) -> RestartReply {
    match reply.result {
        ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(
            LifecycleReply::Restart(view),
        ))) => view,
        other => panic!("restart reply: {other:?}"),
    }
}
fn untouched(core: &ComposedCore) -> serde_json::Value {
    let db = storage::Database::open(core.host.storage_config()).unwrap();
    let binding = core
        .host
        .host
        .lock()
        .unwrap()
        .service()
        .lane_bindings()
        .observe();
    serde_json::json!({
        "transactions": db.restart_transactions().unwrap().iter().map(|t| (&t.metadata, t.phase, t.revision)).collect::<Vec<_>>(),
        "owners": format!("{:?}", core.host.owner.restart_inventory().unwrap()),
        "gate": format!("{:?}", core.host.runtime.quiesce_state()),
        "core": format!("{:?}", core.driver.restart.snapshot()),
        "bindings_current": binding.is_current(),
    })
}
async fn target(core: &ComposedCore) {
    target_in(core.dir.path()).await;
}
async fn target_in(root: &std::path::Path) {
    checked_target::checked_target(root).await;
    let path = root.join("target");
    let wrapper = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        path,
        wrapper.replace(
            "\"protocol\":1",
            &format!(
                "\"protocol\":{}",
                workbench_protocol::handshake::ProtocolVersion::CURRENT.0
            ),
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[core.restart.targets.target]\npath = {:?}\n",
            root.join("target")
        ),
    )
    .unwrap();
}
async fn binding(conn: &mut Conn, operation: &str) -> RestartBinding {
    let RestartReply::Targets(targets) =
        view(conn.call(&query("targets", RestartQuery::Targets)).await)
    else {
        panic!("targets")
    };
    assert_eq!(
        targets
            .targets
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["current", "target"]
    );
    RestartBinding {
        target_id: id("target"),
        expected_core_epoch: targets.core_epoch,
        target_revision: targets.revision,
        operation_id: id(operation),
    }
}

async fn chat(core: &ComposedCore) -> runtime::RunId {
    core.host.owner.start("plain").unwrap();
    let mut events = core.host.runtime.event_bus().subscribe();
    let replies = core.host.host.lock().unwrap().submit_local(
        workbench_host::commands::HostCommand::SendChat(workbench_host::commands::ChatInput {
            submission_id: "root".into(),
            project_root: Some(core.dir.path().canonicalize().unwrap()),
            composer_role: workbench_host::commands::ComposerRole::Orchestrator,
            attachments: vec![],
            thread_id: "plain".into(),
            text: "ordinary".into(),
            skills: vec![],
            model_preference: None,
            fork_seed: None,
        }),
        &Provenance::human(Surface::Desktop, ClientId::new("human").unwrap()),
    );
    let run: runtime::RunId = replies
        .iter()
        .find_map(|r| match r {
            workbench_host::commands::HostEvent::ChatAccepted { run_id, .. } => {
                Some(run_id.parse().unwrap())
            }
            _ => None,
        })
        .unwrap();
    loop {
        if matches!(events.recv().await.unwrap().kind,
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to:event_bus::AgentRunPhase::Waiting, .. }) if run_id == run.to_string())
        {
            break;
        }
    }
    run
}

#[tokio::test]
async fn installed_restart_is_published_and_unauthenticated_changes_never_reach_effects() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    target(&core).await;
    support::seed(&core.host, core.host.storage_config(), true);
    assert_eq!(compose(&core), Publication::Published);
    assert!(core.host.admin.installed_facades().all());
    let socket = core.dir.sockets().join("wire");
    let (stop, server) = serve(
        tokio::net::UnixListener::bind(&socket).unwrap(),
        core.backend.clone(),
    );
    let conn = Conn::open(&socket, "restart-install").await;
    assert!(conn.hello.has(Capability::SafeRestart));
    let before = untouched(&core);
    let binding = RestartBinding {
        target_id: id("target"),
        expected_core_epoch: core.host.admin.epoch(),
        target_revision: Revision(1),
        operation_id: id("published"),
    };
    let request = command(
        "published",
        RestartCommand::Request(RequestRestart {
            binding,
            confirmed: true,
        }),
    );
    let session = WebSession {
        session_id: SessionId::new("session").unwrap(),
        client_id: ClientId::new("web-human").unwrap(),
    };
    for context in [
        workbench_authority::IngressContext::unverified(Provenance::human(
            Surface::Web,
            ClientId::new("web-human").unwrap(),
        )),
        workbench_authority::IngressContext::unverified(Provenance::agent(
            workbench_protocol::ids::RunId::new("agent-run").unwrap(),
        )),
        workbench_authority::IngressContext::system(Provenance::agent(
            workbench_protocol::ids::RunId::new("agent-run").unwrap(),
        )),
    ] {
        let reply = core
            .backend
            .handle_request(&session, request.clone(), context)
            .await;
        assert_eq!(error_of(&reply).0, ErrorCode::PermissionDenied);
    }
    assert_eq!(untouched(&core), before);
    stop.send(true).unwrap();
    server.abort();
    core.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_bindings_remain_fixed_when_hello_projection_is_invalidated() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    let epoch = core.host.admin.epoch();
    core.backend.projection.invalidate();
    assert_ne!(core.backend.projection.instance_epoch(), epoch);
    target(&core).await;
    compose(&core);
    core.host.admin.publish_for_test();
    let evaluations = core.host.restart_preparing_services().evaluations;
    let (release, held) = tokio::sync::oneshot::channel();
    evaluations.spawn(async { held.await.unwrap() }).unwrap();
    let socket = core.dir.sockets().join("wire");
    let (stop, server) = serve(
        tokio::net::UnixListener::bind(&socket).unwrap(),
        core.backend.clone(),
    );
    let mut conn = Conn::open(&socket, "restart-epoch").await;
    let RestartReply::Targets(targets) =
        view(conn.call(&query("targets", RestartQuery::Targets)).await)
    else {
        panic!("targets")
    };
    assert_eq!(targets.core_epoch, epoch);
    let RestartReply::Status(status) =
        view(conn.call(&query("status", RestartQuery::Status)).await)
    else {
        panic!("status")
    };
    assert_eq!(status.core_epoch, epoch);
    let binding = binding(&mut conn, "fixed-epoch-request").await;
    assert_eq!(binding.expected_core_epoch, epoch);
    let reply = conn
        .call(&command(
            "request",
            RestartCommand::Request(RequestRestart {
                binding,
                confirmed: true,
            }),
        ))
        .await;
    assert!(matches!(reply.result, ReplyResult::Success(_)), "{reply:?}");
    let attempt = core.driver.restart.snapshot().id.unwrap();
    core.driver.restart.cancel(attempt).unwrap();
    release.send(()).unwrap();
    stop.send(true).unwrap();
    server.abort();
    core.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_wire_request_deduplicates_and_cancel_before_capture_leaves_no_transaction() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    target(&core).await;
    compose(&core);
    core.host.admin.publish_for_test();
    let evaluations = core.host.restart_preparing_services().evaluations;
    let (release, held) = tokio::sync::oneshot::channel();
    evaluations.spawn(async { held.await.unwrap() }).unwrap();
    let socket = core.dir.sockets().join("wire");
    let (stop, server) = serve(
        tokio::net::UnixListener::bind(&socket).unwrap(),
        core.backend.clone(),
    );
    let mut conn = Conn::open(&socket, "restart-install").await;
    let binding = binding(&mut conn, "restart-once").await;
    let request = command(
        "request",
        RestartCommand::Request(RequestRestart {
            binding,
            confirmed: true,
        }),
    );
    let first = conn.call(&request).await;
    assert!(matches!(first.result, ReplyResult::Success(_)), "{first:?}");
    let snapshot = core.driver.restart.snapshot();
    assert_eq!(
        snapshot.phase,
        crate::restart::RestartPhase::PreparingDrains
    );
    let again = conn.call(&request).await;
    assert!(matches!(again.result, ReplyResult::Success(_)), "{again:?}");
    assert_eq!(core.driver.restart.snapshot().id, snapshot.id);
    let before_interrupt = untouched(&core);
    let interrupt = command("interrupt", RestartCommand::Interrupt(InterruptShell {
        binding: InterruptBinding {
            restart_id: snapshot.id.unwrap().public_id(), core_epoch: core.host.admin.epoch(),
            connection_epoch: conn.hello.instance_epoch, run_incarnation: id("no-run"),
            blocker_identity: id("no-blocker"), revision: Revision(snapshot.revision),
            phase: workbench_protocol::namespaces::lifecycle::restart::RestartPhase::PreparingDrains,
        }, operation_id: id("interrupt"), confirmed: true,
    }));
    assert_eq!(
        error_of(&conn.call(&interrupt).await).0,
        ErrorCode::Conflict
    );
    assert_eq!(untouched(&core), before_interrupt);
    let RestartReply::Status(status) =
        view(conn.call(&query("status", RestartQuery::Status)).await)
    else {
        panic!("status")
    };
    let cancel = command(
        "cancel",
        RestartCommand::Cancel(CancelRestart {
            restart_id: snapshot.id.unwrap().public_id(),
            expected_core_epoch: status.core_epoch,
            expected_revision: Revision(snapshot.revision),
            operation_id: id("cancel"),
        }),
    );
    let result = conn.call(&cancel).await;
    assert!(
        matches!(result.result, ReplyResult::Success(_)),
        "{result:?}"
    );
    assert_eq!(
        core.driver.restart.snapshot().phase,
        crate::restart::RestartPhase::Running
    );
    assert!(
        storage::Database::open(core.host.storage_config())
            .unwrap()
            .restart_transactions()
            .unwrap()
            .is_empty()
    );
    release.send(()).unwrap();
    stop.send(true).unwrap();
    server.abort();
    core.host.shutdown().await.unwrap();
}

mod physical_bootstrap;

#[tokio::test]
async fn live_undisposed_in_the_composed_physical_host_refuses_before_gate_or_transaction() {
    let Some(entry) = physical_bootstrap::take() else {
        let dir = test_socket_dir::SplitDir::new();
        target_in(dir.path()).await;
        let name = std::thread::current().name().unwrap().to_owned();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--"])
            .env(physical_bootstrap::ROOT, dir.path())
            .env(physical_bootstrap::SOCKETS, dir.sockets())
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os(physical_bootstrap::ROOT).unwrap());
    let sockets = std::path::PathBuf::from(std::env::var_os(physical_bootstrap::SOCKETS).unwrap());
    let options = workbench_host::bootstrap::BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: sockets.join("owners"),
    };
    let storage = workbench_host::bootstrap::planned::PlannedStorage::open(options)
        .await
        .unwrap();
    storage
        .runtime()
        .adopt_restart_handoff(entry.proof)
        .unwrap();
    let claimed = storage.claim(physical_bootstrap::TX).unwrap();
    assert!(claimed.blocked_threads.is_empty());
    let mut host = workbench_host::bootstrap::BootstrappedHost::open_claimed_for_core(claimed)
        .await
        .unwrap();
    let old = host.runtime.adopted_restart_transaction().unwrap();
    host.storage_writer()
        .mutate_restart(
            physical_bootstrap::TX.into(),
            Some(old.revision),
            storage::recovery::restart::RestartMutation::Ready { at_ms: 0 },
        )
        .unwrap();
    // Defer automatic graph activation, without replacing its real collector.
    // This test exercises the product leaf composition, not V-exec's product successor.
    host.activate_after_ready(&physical_bootstrap::ParkedRecovery)
        .await
        .unwrap();
    let backend = std::sync::Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        entry.instance.identity.clone(),
    ));
    let driver = crate::lifecycle::LifecycleDriver::new(&host);
    let context = crate::lifecycle::product_exit::ProductExitContext::new(
        entry.instance,
        entry.mask.clone(),
        crate::lifecycle::OperatorSignals::new(&entry.mask).unwrap(),
        driver.control_drain(),
        crate::lifecycle::product_exit::ProductOptions::new(root, false),
    );
    crate::admin_composition::install(
        &Inputs::new(&host, &backend).with_finalization(driver.finalization(), &context),
    );
    assert!(host.admin.installed_facades().all());
    host.admin.publish_for_test();
    let collector = host.graph_collector().clone();
    assert_eq!(collector.snapshot().unwrap().count, Revision(1));
    let heartbeat_rows =
        runtime::ownership::HeartbeatObservations::install_for_test(&host.runtime.event_bus());
    let mut owner_events = host.runtime.event_bus().subscribe();
    let before = physical_bootstrap::state(&host, &driver);
    let renewals = physical_bootstrap::wait_for_renewal(&host, &before, &mut owner_events).await;
    let socket = sockets.join("wire");
    let (stop, server) = serve(tokio::net::UnixListener::bind(&socket).unwrap(), backend);
    let mut conn = Conn::open(&socket, "physical-collector").await;
    let aggregate = conn.call(&envelope("owned", RequestBody::Query(Query::Lifecycle(workbench_protocol::namespaces::lifecycle::LifecycleQuery::OwnedGraphs(workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsQuery::Undisposed))))).await;
    assert!(
        matches!(&aggregate.result, ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(LifecycleReply::OwnedGraphs(workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsReply::Undisposed(graphs))))) if graphs.count == Revision(1))
    );
    let binding = binding(&mut conn, "blocked-by-owned").await;
    let reply = conn
        .call(&command(
            "request",
            RestartCommand::Request(RequestRestart {
                binding,
                confirmed: true,
            }),
        ))
        .await;
    assert_eq!(error_of(&reply).0, ErrorCode::Conflict);
    let after = physical_bootstrap::state(&host, &driver);
    let renewals =
        physical_bootstrap::finish_renewals(&sockets, &before, &mut owner_events, renewals).await;
    before.assert_unchanged_except_lease_renewal(&after, &renewals, &heartbeat_rows.drain());
    assert_eq!(collector.snapshot().unwrap().count, Revision(1));
    stop.send(true).unwrap();
    server.abort();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_binding_during_preparing_drains_cancels_before_park_or_create() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    target(&core).await;
    let root = chat(&core).await;
    compose(&core);
    core.host.admin.publish_for_test();
    let evaluations = core.host.restart_preparing_services().evaluations;
    let (release, held) = tokio::sync::oneshot::channel();
    evaluations.spawn(async { held.await.unwrap() }).unwrap();
    let socket = core.dir.sockets().join("wire");
    let (stop, server) = serve(
        tokio::net::UnixListener::bind(&socket).unwrap(),
        core.backend.clone(),
    );
    let mut conn = Conn::open(&socket, "restart-install").await;
    let binding = binding(&mut conn, "changing-source").await;
    let reply = conn
        .call(&command(
            "request",
            RestartCommand::Request(RequestRestart {
                binding,
                confirmed: true,
            }),
        ))
        .await;
    assert!(matches!(reply.result, ReplyResult::Success(_)), "{reply:?}");
    let id = core.driver.restart.snapshot().id.unwrap();
    core.host
        .host
        .lock()
        .unwrap()
        .service_mut()
        .bind_goal_context("plain", "project", &root.to_string());
    release.send(()).unwrap();
    assert!(core.driver.restart.wait_sealed(id).await.is_err());
    assert_eq!(
        core.driver.restart.snapshot().phase,
        crate::restart::RestartPhase::Running
    );
    assert_eq!(
        core.host.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Running
    );
    assert!(
        storage::Database::open(core.host.storage_config())
            .unwrap()
            .restart_transactions()
            .unwrap()
            .is_empty()
    );
    stop.send(true).unwrap();
    server.abort();
    core.host.shutdown().await.unwrap();
}
