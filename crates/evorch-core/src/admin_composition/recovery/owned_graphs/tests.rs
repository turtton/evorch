//! Q2 installed by the composition root, over a real `BootstrappedHost` that
//! has run its cold recovery and a real Unix listener served by
//! `unix::connection`. The facade is the one `install` builds from the host's
//! own runtime, owner, storage, reader, diagnostics and live collector; the
//! cold graph it observes is read back from the host's database.
//! Synchronization is by replies and joined request slots only. It lives in the
//! tree because the publish-boundary check names `admin_composition` only there.
use super::super::support::{Conn, error_of, id, seed, serve};
use crate::admin_composition::{Inputs, capability::Publication, install};
use crate::{backend::HostBackend, recovery_service::CoreGraphRecovery};
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
        AdminId, GraphCas, LifecycleCommand, LifecycleQuery, LifecycleReply,
        owned_graphs::{
            ActionAvailability, ConfirmedOwnedGraph, OwnedGraphBinding, OwnedGraphView,
            OwnedGraphsCommand, OwnedGraphsQuery, OwnedGraphsReply, UndisposedGraphs,
        },
        quarantine::{QuarantineItem, QuarantineQuery, QuarantineReply},
    },
    queries::{Query, QueryReply},
    wire::{InstanceEpoch, Revision},
};
use workbench_web::{WebSession, WorkbenchBackend};

/// The production composition with Q2 installed, a cold graph (two members on
/// two threads) in its database, and its Unix socket served by the real
/// `unix::connection`.
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
    /// With `recovery`, the host has run its cold recovery, as `main` does.
    async fn start(recovery: bool) -> (Self, Publication) {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let options = BootstrapOptions {
            config_dir: root.clone(),
            project_root: root,
            ownership_root: dir.sockets().to_path_buf(),
        };
        // open_for_core returns once recovery has reached its Ready boundary.
        let boot = if recovery {
            BootstrappedHost::open_for_core(options, &CoreGraphRecovery)
                .await
                .unwrap()
        } else {
            BootstrappedHost::open(options).await.unwrap()
        };
        assert_eq!(boot.recovery_diagnostics.is_some(), recovery);
        let backend = Arc::new(HostBackend::new(boot.host.clone(), "q2-install".into()));
        let config = boot.composition.storage.clone();
        seed(&boot, &config, true);
        let publication = install(&Inputs::new(&boot, &backend));

        let socket = dir.sockets().join("q2-install.sock");
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
    fn untouched(&self) -> (i64, i64, i64, String) {
        let db = rusqlite::Connection::open(&self.config.db_path).unwrap();
        let contents = [
            "recovery_graph_control",
            "recovery_graph_effects",
            "accepted_admin_operations",
        ]
        .map(|table| {
            let mut statement = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    Ok((0..columns)
                        .map(|column| format!("{:?}", row.get_ref(column).unwrap()))
                        .collect::<Vec<_>>())
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows.sort();
            rows
        });
        (
            self.rows("recovery_graph_control"),
            self.rows("recovery_graph_effects"),
            self.rows("accepted_admin_operations"),
            format!("{contents:?}"),
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
        session_id: SessionId::new("q2-session").unwrap(),
        body,
    }
}
fn query(request_id: &str, query: OwnedGraphsQuery) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::OwnedGraphs(query))),
    )
}
fn quarantine_list(request_id: &str) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::Quarantine(
            QuarantineQuery::List,
        ))),
    )
}
fn command(request_id: &str, command: OwnedGraphsCommand) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Command(Command::Lifecycle(LifecycleCommand::OwnedGraphs(command))),
    )
}
fn confirmed(binding: OwnedGraphBinding) -> ConfirmedOwnedGraph {
    ConfirmedOwnedGraph {
        binding,
        confirmed: true,
    }
}
/// A well-formed binding for a graph that does not exist, for the requests that
/// must be refused before anything reads it.
fn any_binding(epoch: u64) -> OwnedGraphBinding {
    OwnedGraphBinding {
        graph: GraphCas {
            graph_id: id("graph.any"),
            restart_tx_id: id("tx.any"),
            capture_id: id("capture.any"),
            expected_revision: Revision(1),
            expected_core_epoch: InstanceEpoch(epoch),
            owner_identity: None,
            owner_generation: None,
            operation_id: id("op-any"),
        },
        owners: Vec::new(),
        expected_control_revision: Revision(0),
    }
}
fn owned_graphs_reply(reply: &ReplyEnvelope) -> OwnedGraphsReply {
    match &reply.result {
        ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(
            LifecycleReply::OwnedGraphs(reply),
        ))) => reply.clone(),
        other => panic!("not an owned-graphs reply: {other:?}"),
    }
}
async fn undisposed(conn: &mut Conn, request_id: &str) -> ReplyEnvelope {
    conn.call(&query(request_id, OwnedGraphsQuery::Undisposed))
        .await
}
async fn detail(conn: &mut Conn, request_id: &str, graph_ref: &AdminId) -> OwnedGraphView {
    let reply = conn
        .call(&query(
            request_id,
            OwnedGraphsQuery::Detail {
                graph_ref: graph_ref.clone(),
            },
        ))
        .await;
    let OwnedGraphsReply::Detail(view) = owned_graphs_reply(&reply) else {
        panic!("detail");
    };
    *view
}
/// The one cold graph, as Q1 lists it. Q1 and Q2 name a graph by the same
/// reference, so it is also Q2's handle on it.
async fn quarantined(conn: &mut Conn, request_id: &str) -> QuarantineItem {
    let reply = conn.call(&quarantine_list(request_id)).await;
    let ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(
        LifecycleReply::Quarantine(QuarantineReply::List { mut items, .. }),
    ))) = reply.result
    else {
        panic!("quarantine list");
    };
    assert_eq!(items.len(), 1);
    items.remove(0)
}

/// Registered is not published: every Q2 request from a verified local client
/// is refused before the facade, the audit journal or the store is reached,
/// and the hello offers nothing new.
#[tokio::test]
async fn an_installed_leaf_refuses_every_request_while_management_is_off() {
    let (core, publication) = Installed::start(true).await;
    assert_eq!(publication, Publication::Off);
    let installed = core.boot.admin.installed_facades();
    assert!(installed.owned_graphs && installed.quarantine);
    assert_eq!(installed.restart, cfg!(not(target_os = "linux")));
    assert_eq!(installed.all(), cfg!(not(target_os = "linux")));
    assert!(!core.backend.hello().has(Capability::SafeRestart));

    let mut conn = Conn::open(&core.socket, "q2-install").await;
    assert!(!conn.hello.has(Capability::SafeRestart));
    let before = core.untouched();
    let epoch = conn.hello.instance_epoch.get();
    let requests = [
        query("r-undisposed", OwnedGraphsQuery::Undisposed),
        query(
            "r-detail",
            OwnedGraphsQuery::Detail {
                graph_ref: id("graph.any"),
            },
        ),
        query(
            "r-operation",
            OwnedGraphsQuery::Operation {
                operation_id: id("op-any"),
            },
        ),
        command(
            "r-release",
            OwnedGraphsCommand::Release(confirmed(any_binding(epoch))),
        ),
        command(
            "r-resume",
            OwnedGraphsCommand::Resume(confirmed(any_binding(epoch))),
        ),
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
/// cold graph over the wire through the host's own collector; an unverified Web
/// or Agent caller cannot release or resume it, and a verified local request
/// reaches the facade, which refuses a cold graph.
#[tokio::test]
async fn the_installed_leaf_serves_a_verified_local_client_and_refuses_web_and_agent() {
    let (core, _) = Installed::start(true).await;
    core.boot.admin.publish_for_test();
    let mut conn = Conn::open(&core.socket, "q2-install").await;
    let epoch = conn.hello.instance_epoch.get();

    // The cold graph is isolated: never part of the live undisposed set.
    let reply = undisposed(&mut conn, "r-undisposed").await;
    let OwnedGraphsReply::Undisposed(UndisposedGraphs {
        count, references, ..
    }) = owned_graphs_reply(&reply)
    else {
        panic!("undisposed");
    };
    assert_eq!((count, references.len()), (Revision(0), 0));

    // Detail reaches the same graph Q1 lists, bound to the epoch of the hello,
    // and neither disposal action is offered on a cold graph.
    let item = quarantined(&mut conn, "r-quarantined").await;
    let view = detail(&mut conn, "r-detail", &item.graph_id).await;
    assert_eq!(view.graph_ref, item.graph_id);
    assert_eq!(view.binding.graph.expected_core_epoch.get(), epoch);
    assert!(!matches!(view.release, ActionAvailability::Available));
    assert!(!matches!(view.resume, ActionAvailability::Available));
    let mut binding = view.binding.clone();
    binding.graph.operation_id = id("op-release");

    // An unverified Web human and an Agent cannot release or resume.
    let session = WebSession {
        session_id: SessionId::new("q2-session").unwrap(),
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
        for action in [
            OwnedGraphsCommand::Release(confirmed(binding.clone())),
            OwnedGraphsCommand::Resume(confirmed(binding.clone())),
        ] {
            let reply = core
                .backend
                .handle_request(&session, command("op-denied", action), context.clone())
                .await;
            assert_eq!(error_of(&reply).0, ErrorCode::PermissionDenied);
        }
    }
    assert_eq!(core.untouched(), before);

    // The verified local operator is let in, and the facade refuses the cold
    // graph without recording a disposal.
    let refused = conn
        .call(&command(
            "op-release",
            OwnedGraphsCommand::Release(confirmed(binding)),
        ))
        .await;
    assert_eq!(error_of(&refused).0, ErrorCode::Conflict);
    assert_eq!(core.rows("recovery_graph_control"), before.0);
    assert_eq!(core.rows("recovery_graph_effects"), before.1);

    // The leaf reads the host's own collector: once that is invalidated, the
    // undisposed query is unavailable rather than answered from a copy.
    core.boot.graph_collector().invalidate();
    let reply = undisposed(&mut conn, "r-after-invalidate").await;
    assert_eq!(error_of(&reply).0, ErrorCode::StorageUnavailable);
    core.shutdown().await;
}

/// Without a started recovery the leaf has nothing to ask, so it stays
/// unregistered: even with the switch on, the service answers "not installed".
#[tokio::test]
async fn a_host_that_has_not_run_recovery_leaves_the_leaf_unregistered() {
    let (core, publication) = Installed::start(false).await;
    assert_eq!(publication, Publication::Off);
    let installed = core.boot.admin.installed_facades();
    assert!(!installed.owned_graphs && installed.quarantine);
    core.boot.admin.publish_for_test();
    let mut conn = Conn::open(&core.socket, "q2-install").await;
    let reply = undisposed(&mut conn, "r-undisposed").await;
    assert_eq!(
        error_of(&reply),
        (
            ErrorCode::UnsupportedCapability,
            "management leaf is not installed"
        )
    );
    core.shutdown().await;
}

/// A cold graph is discarded only when Q2's progress write has recorded every
/// thread (#723): the discard leaves it partial and failed, and Q1 and Q2 both
/// report it complete only after the last thread is written. Management effects
/// run on Linux only, so the discard is not exercised elsewhere.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cold_graph_is_complete_only_after_every_thread_is_recorded() {
    use super::super::support::{MANIFEST, TX};
    use storage::{
        Database,
        admin_audit::AuditOutcome,
        recovery::graph_progress::{
            GraphDisposalProgressKey, GraphDisposalStep, GraphThreadProgressWrite,
        },
    };
    use workbench_protocol::namespaces::lifecycle::{
        ConfirmedGraph,
        owned_graphs::{DisposalCompletion, StepObservation},
        quarantine::{QuarantineCommand, QuarantineCompletion, QuarantineEligibility},
    };
    let (core, _) = Installed::start(true).await;
    core.boot.admin.publish_for_test();
    let mut conn = Conn::open(&core.socket, "q2-install").await;
    let epoch = conn.hello.instance_epoch.get();
    let item = quarantined(&mut conn, "r-list").await;
    assert_eq!(item.completion, QuarantineCompletion::NotStarted);
    let view = detail(&mut conn, "r-detail", &item.graph_id).await;
    assert_eq!(view.progress.completion, DisposalCompletion::NotStarted);

    // Q1's discard revokes the eligibility. The threads have no disposal
    // record, so it is a failure and the graph stays partial.
    let discard = envelope(
        "op-discard",
        RequestBody::Command(Command::Lifecycle(LifecycleCommand::Quarantine(
            QuarantineCommand::Discard(ConfirmedGraph {
                binding: GraphCas {
                    graph_id: item.graph_id.clone(),
                    restart_tx_id: item.restart_tx_id.clone(),
                    capture_id: item.capture_id.clone(),
                    expected_revision: item.revision,
                    expected_core_epoch: InstanceEpoch(epoch),
                    owner_identity: None,
                    owner_generation: None,
                    operation_id: id("op-discard"),
                },
                confirmed: true,
            }),
        ))),
    );
    let accepted = conn.call(&discard).await;
    assert!(matches!(
        accepted.result,
        ReplyResult::Success(ReplyPayload::Command(_))
    ));
    core.boot.admin.wait_for_request_slots().await;
    let audit = Database::read_admin_audit_record(&core.config, "op-discard")
        .unwrap()
        .and_then(|record| record.outcome);
    assert_eq!(audit, Some(AuditOutcome::Failed));
    let revoked = quarantined(&mut conn, "r-revoked").await;
    assert_eq!(revoked.eligibility, QuarantineEligibility::Revoked);
    assert_eq!(revoked.completion, QuarantineCompletion::Partial);
    assert!(!revoked.discard_complete());
    let view = detail(&mut conn, "r-detail-revoked", &item.graph_id).await;
    assert_eq!(view.progress.completion, DisposalCompletion::Partial);

    // The production progress write, one thread at a time, through the host's
    // own writer. One thread is not enough.
    let control = Database::open(&core.config)
        .unwrap()
        .recovery_graph_control(MANIFEST)
        .unwrap()
        .unwrap();
    let record = |thread: &str| GraphThreadProgressWrite {
        key: GraphDisposalProgressKey {
            manifest_id: MANIFEST.into(),
            restart_id: TX.into(),
            capture_id: "capture".into(),
            thread_id: thread.into(),
            owner_generation: 1,
        },
        expected_revision: 0,
        control: control.clone(),
        owners: GraphDisposalStep::NotApplicable,
        scratch: GraphDisposalStep::Applied,
    };
    core.boot
        .storage_writer()
        .write_recovery_graph_progress(record("run-a"))
        .unwrap();
    let one = quarantined(&mut conn, "r-one").await;
    assert!(!one.discard_complete());
    let view = detail(&mut conn, "r-detail-one", &item.graph_id).await;
    assert_eq!(view.progress.completion, DisposalCompletion::Partial);

    core.boot
        .storage_writer()
        .write_recovery_graph_progress(record("run-b"))
        .unwrap();
    let done = quarantined(&mut conn, "r-done").await;
    assert!(done.discard_complete());
    let view = detail(&mut conn, "r-detail-done", &item.graph_id).await;
    assert_eq!(view.progress.completion, DisposalCompletion::Complete);
    assert_eq!(view.progress.owners, StepObservation::NotApplicable);
    assert_eq!(view.progress.scratch, StepObservation::ConfirmedApplied);
    core.shutdown().await;
}
