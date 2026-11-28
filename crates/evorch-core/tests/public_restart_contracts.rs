//! 5a-I common contract, core transport half. Every scenario enters through a
//! real `UnixListener` served by the real `unix::connection`; the leaf facade
//! and the audit writer are fakes (`support`). Synchronization is by events,
//! channels and joins only.

#[path = "public_restart_contracts/support.rs"]
mod support;

use serde_json::json;
use storage::admin_audit::{AuditOperation, AuditOutcome, AuditState, AuditSurface};
use support::*;
use workbench_authority::IngressContext;
use workbench_protocol::{
    ErrorCode,
    envelope::{Provenance, Surface},
    handshake::{Capability, HandshakeReply, LocalSurface, RejectReason},
    ids::{CapabilityName, ClientId, RunId, SessionId},
    namespaces::lifecycle::{
        AdminOperationState, AdminOutcome, LifecycleQuery, LifecycleReply,
        restart::{RestartQuery, RestartReply},
    },
    queries::Query,
};
use workbench_web::{WebSession, WorkbenchBackend};

fn euid() -> String {
    rustix::process::geteuid().as_raw().to_string()
}

#[tokio::test]
async fn cancel_and_status_overtake_a_held_general_request_on_the_same_connection() {
    // Given: one connection whose general restart request is parked on its audit ACK.
    let mut core = Core::start(true).await;
    core.port.hold_accept_of("held-restart");
    let mut conn = core.connect(&client_hello("same-connection", None)).await;
    let epoch = conn.hello.instance_epoch;
    conn.send(&restart_request(epoch, "r-held", "held-restart"))
        .await;
    core.event("accept", "held-restart").await;

    // When: cancel and status follow on that very connection.
    conn.send(&cancel_request(epoch, "r-cancel", "cancel-1"))
        .await;
    conn.send(&status_request("r-status")).await;

    // Then: both answer while the general request is still unanswered.
    let first = conn.reply().await;
    assert_eq!(first.request_id.as_str(), "r-cancel");
    let cancelled = receipt(&first);
    assert_eq!(cancelled.outcome, AdminOutcome::Cancelled);
    assert_eq!(cancelled.state, AdminOperationState::Completed);
    assert!(cancelled.audit_warning, "a cancel never claims durability");
    let second = conn.reply().await;
    assert_eq!(second.request_id.as_str(), "r-status");
    assert!(matches!(
        lifecycle_view(&second),
        LifecycleReply::Restart(RestartReply::Status(_))
    ));
    let parked = core.port.record("held-restart").unwrap();
    assert_eq!(parked.state, AuditState::Prepared);
    assert_eq!(
        core.facade
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing past the held ACK ran"
    );

    // When: the ACK is released.
    core.port.release();

    // Then: the general request completes after them, and runs its effect once.
    let third = conn.reply().await;
    assert_eq!(third.request_id.as_str(), "r-held");
    let accepted = receipt(&third);
    assert_eq!(accepted.state, AdminOperationState::Started);
    core.event("effect", "held-restart").await;
    core.event("finalize", "held-restart").await;
    core.event("finalize", "cancel-1").await;
    assert_eq!(
        core.facade
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let held = core.port.record("held-restart").unwrap();
    assert_eq!(
        (held.state, held.outcome),
        (AuditState::Finished, Some(AuditOutcome::Succeeded))
    );
    let cancel = core.port.record("cancel-1").unwrap();
    assert_eq!(cancel.outcome, Some(AuditOutcome::Cancelled));
    core.shutdown().await;
}

#[tokio::test]
async fn a_peer_uid_verified_local_connection_is_a_human_desktop_or_cli_administrator() {
    let mut core = Core::start(true).await;
    for (name, claimed, surface) in [
        ("desktop-human", None, AuditSurface::Desktop),
        ("cli-human", Some(LocalSurface::Cli), AuditSurface::Cli),
    ] {
        let mut conn = core.connect(&client_hello(name, claimed)).await;
        let epoch = conn.hello.instance_epoch;
        let operation = format!("op-{name}");

        // When: the verified peer queries and requests.
        conn.send(&query_request(
            "r-targets",
            LifecycleQuery::Restart(RestartQuery::Targets),
        ))
        .await;
        let targets = lifecycle_view(&conn.reply().await);
        assert!(matches!(
            targets,
            LifecycleReply::Restart(RestartReply::Targets(_))
        ));
        conn.send(&status_request("r-status")).await;
        assert!(matches!(
            lifecycle_view(&conn.reply().await),
            LifecycleReply::Restart(RestartReply::Status(_))
        ));
        conn.send(&restart_request(epoch, "r-restart", &operation))
            .await;
        let reply = conn.reply().await;

        // Then: it is admitted, and the durable principal is the kernel-verified
        // peer, not anything the payload claimed.
        assert_eq!(receipt(&reply).operation_id.as_str(), operation);
        core.event("finalize", &operation).await;
        let record = core.port.record(&operation).unwrap();
        assert_eq!(record.accepted.operation, AuditOperation::RestartRequest);
        assert_eq!(record.accepted.surface, surface);
        assert_eq!(record.accepted.principal.method, "unix-peer-uid");
        assert_eq!(record.accepted.principal.subject, euid());
        assert!(!record.accepted.principal.binding.is_empty());
    }
    assert_eq!(
        core.facade
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    core.shutdown().await;
}

#[tokio::test]
async fn a_client_cannot_supply_provenance_on_the_wire() {
    let core = Core::start(true).await;
    let mut conn = core.connect(&client_hello("forger", None)).await;
    let epoch = conn.hello.instance_epoch;

    // A stray envelope field claiming another actor is refused, not ignored.
    let mut stray = serde_json::to_value(restart_request(epoch, "r-stray", "forged-1")).unwrap();
    stray["provenance"] = json!({"actor": "human", "surface": "cli", "client_id": "core-health"});
    conn.send_raw(&stray).await;
    let reply = conn.reply().await;
    assert_eq!(reply.request_id.as_str(), "r-stray");
    assert_eq!(error_code(&reply), ErrorCode::InvalidRequest);

    // So is an Agent claim smuggled into the command payload.
    let mut nested = serde_json::to_value(restart_request(epoch, "r-nested", "forged-2")).unwrap();
    nested["payload"]["payload"]["payload"]["provenance"] =
        json!({"actor": "agent", "surface": "none", "agent_run_id": "run-1"});
    conn.send_raw(&nested).await;
    let reply = conn.reply().await;
    assert_eq!(reply.request_id.as_str(), "r-nested");
    assert_eq!(error_code(&reply), ErrorCode::InvalidRequest);

    // Nothing reached the leaf or the audit writer, and the connection is intact.
    assert_eq!(
        core.facade.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(core.port.row_count(), 0);
    assert_eq!(core.port.write_count(), 0);
    conn.send(&status_request("r-after")).await;
    assert!(matches!(
        lifecycle_view(&conn.reply().await),
        LifecycleReply::Restart(RestartReply::Status(_))
    ));
    core.shutdown().await;
}

#[tokio::test]
async fn a_forged_hello_never_becomes_a_connection() {
    let mut core = Core::start(true).await;
    let genuine = serde_json::to_value(client_hello("forged-hello", None)).unwrap();
    let forgeries = {
        let mut web = genuine.clone();
        web["local_surface"] = json!("web");
        let mut provenance = genuine.clone();
        provenance["provenance"] = json!({"actor": "human", "surface": "desktop"});
        let mut actor = genuine.clone();
        actor["actor"] = json!("agent");
        [web, provenance, actor]
    };
    for forged in forgeries {
        // When: the first frame claims a surface or a provenance the wire has no field for.
        let mut stream = tokio::net::UnixStream::connect(&core.socket).await.unwrap();
        write_value(&mut stream, &forged).await;

        // Then: the core ends the connection without a handshake reply.
        assert!(guard(read_frame(&mut stream)).await.is_none(), "{forged}");
        assert!(!core.next_ended().await);
    }
    assert_eq!(
        core.facade.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(core.port.write_count(), 0);
    core.shutdown().await;
}

#[tokio::test]
async fn unverified_agent_system_and_core_health_paths_cannot_administer() {
    let core = Core::start(true).await;
    let epoch = core.epoch;
    // The provenance the core's own status query is made with.
    let core_health = Provenance::human(Surface::Cli, ClientId::new("core-health").unwrap());
    let contexts = [
        IngressContext::unverified(core_health.clone()),
        IngressContext::system(core_health.clone()),
        IngressContext::unverified(Provenance::human(
            Surface::Desktop,
            ClientId::new("claims-to-be-human").unwrap(),
        )),
        IngressContext::unverified(Provenance::agent(RunId::new("agent-run").unwrap())),
        IngressContext::system(Provenance::agent(RunId::new("agent-run").unwrap())),
    ];
    let session = WebSession {
        session_id: SessionId::new("contracts-session").unwrap(),
        client_id: ClientId::new("core-health").unwrap(),
    };
    let requests = [
        restart_request(epoch, "r-restart", "denied-restart"),
        cancel_request(epoch, "r-cancel", "denied-cancel"),
        status_request("r-status"),
        query_request("r-targets", LifecycleQuery::Restart(RestartQuery::Targets)),
        query_request(
            "r-operation",
            LifecycleQuery::OperationStatus {
                operation_id: id("denied-restart"),
            },
        ),
    ];

    for context in &contexts {
        for request in &requests {
            let reply = core
                .backend
                .handle_request(&session, request.clone(), context.clone())
                .await;
            assert_eq!(
                error_code(&reply),
                ErrorCode::PermissionDenied,
                "{context:?} {request:?}"
            );
        }
    }
    // The plain-Provenance convenience API and the host's own entry points
    // never carry a proof either.
    for request in &requests {
        let reply = core.backend.request(request.clone(), core_health.clone());
        assert_eq!(error_code(&reply), ErrorCode::PermissionDenied);
    }
    {
        let host = core.host.lock().unwrap();
        for query in [
            LifecycleQuery::Restart(RestartQuery::Status),
            LifecycleQuery::Restart(RestartQuery::Targets),
        ] {
            let denied = host
                .handle_query(
                    &SessionId::new("core-health").unwrap(),
                    &Query::Lifecycle(query),
                    &core_health,
                )
                .unwrap_err();
            assert_eq!(denied.code, ErrorCode::PermissionDenied);
        }
    }
    {
        let mut host = core.host.lock().unwrap();
        let workbench_protocol::envelope::RequestBody::Command(command) =
            restart_request(epoch, "r-direct", "denied-direct").body
        else {
            unreachable!()
        };
        let denied = host
            .handle_command(command, core_health.clone())
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
    }

    // Nothing reached the leaf or the audit writer; the status of the core
    // itself, which the health path is for, still answers.
    assert_eq!(
        core.facade.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(core.port.row_count(), 0);
    assert_eq!(
        core.facade
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(core.backend.hello().instance_epoch, epoch);
    core.shutdown().await;
}

#[tokio::test]
async fn a_request_whose_connection_ended_loses_its_proof_before_any_effect() {
    // Given: a general request parked on its audit ACK.
    let mut core = Core::start(true).await;
    core.port.hold_accept_of("orphan");
    let mut conn = core.connect(&client_hello("leaves", None)).await;
    conn.send(&restart_request(
        conn.hello.instance_epoch,
        "r-orphan",
        "orphan",
    ))
    .await;
    core.event("accept", "orphan").await;

    // When: the verified connection ends, and only then the ACK arrives.
    drop(conn);
    core.next_ended().await;
    core.port.release();

    // Then: the local proof lived exactly as long as that connection, so the
    // operation ends before any effect and is recorded as such.
    core.event("finalize", "orphan").await;
    let row = core.port.record("orphan").unwrap();
    assert_eq!(
        (row.state, row.outcome),
        (
            AuditState::Finished,
            Some(AuditOutcome::AbortedBeforeEffect)
        )
    );
    assert_eq!(
        core.facade
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    core.shutdown().await;
}

#[tokio::test]
async fn safe_restart_is_not_advertised_to_any_local_client() {
    let core = Core::start(true).await;
    assert!(!Capability::IMPLEMENTED.contains(&Capability::SafeRestart));
    assert!(!core.backend.hello().has(Capability::SafeRestart));
    let safe_restart = CapabilityName::new("safe_restart.v1").unwrap();

    for surface in [None, Some(LocalSurface::Cli)] {
        // Asking for it as optional negotiates nothing...
        let mut hello = client_hello("wants-safe-restart", surface);
        hello.optional_capabilities = vec![safe_restart.clone()];
        let conn = core.connect(&hello).await;
        assert!(!conn.hello.has(Capability::SafeRestart));
        assert!(conn.hello.has(Capability::Commands));

        // ...and requiring it is refused at the handshake.
        let mut hello = client_hello("requires-safe-restart", surface);
        hello.required_capabilities = vec![safe_restart.clone()];
        let Err(refusal) = Conn::open(&core.socket, &hello).await else {
            panic!("a client requiring safe_restart.v1 was accepted");
        };
        let HandshakeReply::Rejected(rejected) = *refusal else {
            panic!("the refusal is a rejection");
        };
        assert_eq!(rejected.reason, RejectReason::MissingCapabilities);
    }
    core.shutdown().await;
}

#[tokio::test]
async fn without_a_registered_leaf_every_management_request_is_unsupported_before_any_side_effect()
{
    // Given: the product state of this PR, the common service with no leaf.
    let core = Core::start(false).await;
    let mut conn = core.connect(&client_hello("unsupported", None)).await;
    let epoch = conn.hello.instance_epoch;

    // The Q2 integration port must not implicitly install its later leaf facade.
    use workbench_protocol::namespaces::lifecycle::{LifecycleCommand, owned_graphs::*};
    use workbench_protocol::{commands::Command, envelope::RequestBody};
    let graph: ConfirmedOwnedGraph = serde_json::from_value(json!({
        "binding": {
            "graph": {"graph_id":"graph", "restart_tx_id":"tx", "capture_id":"capture",
                "expected_revision":"1", "expected_core_epoch":epoch,
                "operation_id":"unsupported-owned"},
            "threads":[], "expected_control_revision":"0"
        }, "confirmed":true
    }))
    .unwrap();
    let owned_request = |id, command| {
        envelope(
            id,
            RequestBody::Command(Command::Lifecycle(LifecycleCommand::OwnedGraphs(command))),
        )
    };
    // When: a verified local client uses each kind of management request.
    let requests = [
        owned_request("r-release", OwnedGraphsCommand::Release(graph.clone())),
        owned_request("r-resume", OwnedGraphsCommand::Resume(graph)),
        restart_request(epoch, "r-restart", "unsupported-restart"),
        cancel_request(epoch, "r-cancel", "unsupported-cancel"),
        status_request("r-status"),
        query_request("r-targets", LifecycleQuery::Restart(RestartQuery::Targets)),
    ];
    for request in &requests {
        conn.send(request).await;
        let reply = conn.reply().await;

        // Then: authority passed, so it is Unsupported (not a permission error).
        assert_eq!(reply.request_id, request.request_id);
        assert_eq!(error_code(&reply), ErrorCode::UnsupportedCapability);
        assert_eq!(
            error_message(&reply),
            "management leaf is not installed",
            "the request was refused for the missing leaf, not the handshake"
        );
    }
    assert_eq!(core.port.row_count(), 0);
    assert_eq!(core.port.write_count(), 0);
    assert_eq!(
        core.facade.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    core.shutdown().await;
}

#[tokio::test]
async fn projection_cursor_invalidation_does_not_invalidate_management_cas() {
    let mut core = Core::start(true).await;
    core.host.lock().unwrap().projection().invalidate();
    let mut conn = core.connect(&client_hello("epoch-split", None)).await;
    assert_ne!(conn.hello.instance_epoch, core.epoch);
    conn.send(&query_request(
        "targets",
        LifecycleQuery::Restart(RestartQuery::Targets),
    ))
    .await;
    let reply = conn.reply().await;
    let LifecycleReply::Restart(RestartReply::Targets(targets)) = lifecycle_view(&reply) else {
        panic!("targets")
    };
    assert_eq!(targets.core_epoch, core.epoch);
    conn.send(&status_request("management-status")).await;
    let reply = conn.reply().await;
    let LifecycleReply::Restart(RestartReply::Status(status)) = lifecycle_view(&reply) else {
        panic!("status")
    };
    assert_eq!(status.core_epoch, core.epoch);
    assert_eq!(
        status.audit,
        Default::default(),
        "host diagnostic values override untrusted facade values"
    );
    conn.send(&restart_request(
        targets.core_epoch,
        "management-request",
        "management-operation",
    ))
    .await;
    assert_eq!(
        receipt(&conn.reply().await).state,
        AdminOperationState::Started
    );
    core.event("finalize", "management-operation").await;
    conn.send(&restart_request(
        conn.hello.instance_epoch,
        "wrong-cursor",
        "wrong-cursor-operation",
    ))
    .await;
    assert_eq!(error_code(&conn.reply().await), ErrorCode::Conflict);
    assert!(core.port.record("wrong-cursor-operation").is_none());
    drop(conn);
    core.shutdown().await;
}
