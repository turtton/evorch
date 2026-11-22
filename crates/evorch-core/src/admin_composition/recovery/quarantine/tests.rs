//! Q1 installed by the composition root, over a real `BootstrappedHost` and a
//! real Unix listener served by `unix::connection`. Nothing is faked: the
//! facade is the one `install` builds from the host's own storage, and the
//! quarantined graph is read back from that database. Synchronization is by
//! replies and joined request slots only. It lives in the tree because the
//! publish-boundary check names `admin_composition` only there.
use super::super::support::{Conn, error_of, id, seed, serve};
use crate::admin_composition::{Inputs, capability::Publication, install};
use crate::backend::HostBackend;
use std::{path::PathBuf, sync::Arc};
use storage::StorageConfig;
use tokio::sync::watch;
use workbench_authority::IngressContext;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_protocol::{
    commands::Command,
    envelope::{
        Provenance, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope, Surface,
    },
    errors::ErrorCode,
    handshake::Capability,
    ids::{ClientId, RequestId, RunId, SessionId},
    namespaces::lifecycle::{
        ConfirmedGraph, GraphCas, LifecycleCommand, LifecycleQuery, LifecycleReply,
        quarantine::{QuarantineCommand, QuarantineItem, QuarantineQuery, QuarantineReply},
    },
    queries::{Query, QueryReply},
};
use workbench_web::{WebSession, WorkbenchBackend};

/// The production composition with Q1 installed, a quarantined cold graph in
/// its database, and its Unix socket served by the real `unix::connection`.
struct Installed {
    _dir: test_socket_dir::SplitDir,
    boot: BootstrappedHost,
    backend: Arc<HostBackend>,
    config: StorageConfig,
    socket: PathBuf,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
}
impl Installed {
    async fn start() -> (Self, Publication) {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root,
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = Arc::new(HostBackend::new(boot.host.clone(), "q1-install".into()));
        let config = boot.composition.storage.clone();
        seed(&boot, &config, false);
        let publication = install(&Inputs::new(&boot, &backend));

        let socket = dir.sockets().join("q1-install.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, server) = serve(listener, backend.clone());
        let core = Self {
            _dir: dir,
            boot,
            backend,
            config,
            socket,
            stop,
            server,
        };
        (core, publication)
    }
    fn rows(&self, table: &str) -> i64 {
        rusqlite::Connection::open(&self.config.db_path)
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    /// The storage-side state a refused request must leave untouched.
    fn untouched(&self) -> (i64, i64, i64) {
        (
            self.rows("recovery_graph_control"),
            self.rows("recovery_graph_effects"),
            self.rows("accepted_admin_operations"),
        )
    }
    async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        self.server.abort();
        self.boot.shutdown().await.unwrap();
    }
}

fn envelope(request_id: &str, body: RequestBody) -> RequestEnvelope {
    RequestEnvelope {
        request_id: RequestId::new(request_id).unwrap(),
        session_id: SessionId::new("q1-session").unwrap(),
        body,
    }
}
fn query(request_id: &str, query: QuarantineQuery) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::Quarantine(query))),
    )
}
fn discard(request_id: &str, item: &QuarantineItem, epoch: u64) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Command(Command::Lifecycle(LifecycleCommand::Quarantine(
            QuarantineCommand::Discard(ConfirmedGraph {
                binding: GraphCas {
                    graph_id: item.graph_id.clone(),
                    restart_tx_id: item.restart_tx_id.clone(),
                    capture_id: item.capture_id.clone(),
                    expected_revision: item.revision,
                    expected_core_epoch: workbench_protocol::wire::InstanceEpoch(epoch),
                    owner_identity: None,
                    owner_generation: None,
                    operation_id: id(request_id),
                },
                confirmed: true,
            }),
        ))),
    )
}
fn quarantine_reply(reply: &ReplyEnvelope) -> QuarantineReply {
    match &reply.result {
        ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(
            LifecycleReply::Quarantine(reply),
        ))) => reply.clone(),
        other => panic!("not a quarantine reply: {other:?}"),
    }
}
async fn only_item(conn: &mut Conn, request_id: &str) -> QuarantineItem {
    let reply = conn.call(&query(request_id, QuarantineQuery::List)).await;
    let QuarantineReply::List { mut items, .. } = quarantine_reply(&reply) else {
        panic!("list");
    };
    assert_eq!(items.len(), 1);
    items.remove(0)
}

/// Registered is not published: while a leaf is missing, every Q1 request from a verified local client
/// is refused before the facade, the audit journal or the store is reached,
/// and the hello offers nothing new.
#[tokio::test]
async fn an_installed_leaf_refuses_every_request_while_the_composition_is_incomplete() {
    let (core, publication) = Installed::start().await;
    assert!(
        matches!(publication, Publication::Blocked(_)),
        "{publication:?}"
    );
    let installed = core.boot.admin.installed_facades();
    assert!(installed.quarantine && !installed.all());
    assert!(!core.backend.hello().has(Capability::SafeRestart));

    let mut conn = Conn::open(&core.socket, "q1-install").await;
    assert!(!conn.hello.has(Capability::SafeRestart));
    let before = core.untouched();
    let any = QuarantineItem {
        graph_id: id("graph.any"),
        revision: workbench_protocol::wire::Revision(1),
        cold: true,
        restart_tx_id: id("tx.any"),
        capture_id: id("capture.any"),
        reason: workbench_protocol::namespaces::lifecycle::quarantine::QuarantineReason::ColdBoot,
        eligibility:
            workbench_protocol::namespaces::lifecycle::quarantine::QuarantineEligibility::Open,
        claim: workbench_protocol::namespaces::lifecycle::quarantine::QuarantineClaim::Prepared,
        completion:
            workbench_protocol::namespaces::lifecycle::quarantine::QuarantineCompletion::NotStarted,
        members: 2,
    };
    let requests = [
        query("r-list", QuarantineQuery::List),
        query(
            "r-detail",
            QuarantineQuery::Detail {
                graph_id: any.graph_id.clone(),
            },
        ),
        discard("r-discard", &any, conn.hello.instance_epoch.get()),
    ];
    for request in &requests {
        let reply = conn.call(request).await;
        assert_eq!(
            error_of(&reply),
            (
                ErrorCode::UnsupportedCapability,
                "management is not published"
            )
        );
    }
    assert_eq!(core.untouched(), before);
    core.shutdown().await;
}

/// With the switch on in the assembled host, the installed leaf serves the
/// quarantined graph over the wire; an unverified Web or Agent caller still
/// cannot change it, and a verified local confirmed discard does.
#[tokio::test]
async fn the_installed_leaf_serves_a_verified_local_client_and_refuses_web_and_agent() {
    let (core, _) = Installed::start().await;
    core.boot.admin.publish_for_test();
    let mut conn = Conn::open(&core.socket, "q1-install").await;
    let epoch = conn.hello.instance_epoch.get();

    // List and detail agree, and the detail binds the epoch the hello carries.
    let item = only_item(&mut conn, "r-list").await;
    assert!(item.cold && !item.discard_complete());
    let detail = conn
        .call(&query(
            "r-detail",
            QuarantineQuery::Detail {
                graph_id: item.graph_id.clone(),
            },
        ))
        .await;
    let QuarantineReply::Detail(found) = quarantine_reply(&detail) else {
        panic!("detail");
    };
    assert_eq!(found.item, item);
    assert_eq!(found.core_epoch.get(), epoch);

    // An unverified Web human and an Agent cannot discard, however they ask.
    let session = WebSession {
        session_id: SessionId::new("q1-session").unwrap(),
        client_id: ClientId::new("web-human").unwrap(),
    };
    let before = core.untouched();
    let contexts = [
        IngressContext::unverified(Provenance::human(
            Surface::Web,
            ClientId::new("web-human").unwrap(),
        )),
        IngressContext::unverified(Provenance::agent(RunId::new("agent-run").unwrap())),
        IngressContext::system(Provenance::agent(RunId::new("agent-run").unwrap())),
    ];
    for context in contexts {
        let reply = core
            .backend
            .handle_request(&session, discard("op-denied", &item, epoch), context)
            .await;
        assert_eq!(error_of(&reply).0, ErrorCode::PermissionDenied);
    }
    assert_eq!(core.untouched(), before);
    assert!(
        !only_item(&mut conn, "r-after-denied")
            .await
            .discard_complete()
    );

    // The verified local operator's confirmed discard goes through where the
    // management effects run (Linux); elsewhere the service refuses the effect
    // and the graph stays as it was.
    let accepted = conn.call(&discard("op-discard", &item, epoch)).await;
    #[cfg(target_os = "linux")]
    {
        assert!(matches!(
            accepted.result,
            ReplyResult::Success(ReplyPayload::Command(_))
        ));
        core.boot.admin.wait_for_request_slots().await;
        let after = only_item(&mut conn, "r-after").await;
        assert!(after.discard_complete());
        assert!(after.revision > item.revision);
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(
            error_of(&accepted),
            (
                ErrorCode::UnsupportedCapability,
                "management effects require Linux"
            )
        );
        let after = only_item(&mut conn, "r-after").await;
        assert_eq!(after, item);
    }
    core.shutdown().await;
}
