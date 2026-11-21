//! Q1 installed by the composition root, over a real `BootstrappedHost` and a
//! real Unix listener served by `unix::connection`. Nothing is faked: the
//! facade is the one `install` builds from the host's own storage, and the
//! quarantined graph is read back from that database. Synchronization is by
//! replies and joined request slots only. It lives in the tree because the
//! publish-boundary check names `admin_composition` only there.
use crate::admin_composition::{Inputs, capability::Publication, install};
use crate::{backend::HostBackend, unix};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use storage::{
    Database, RunContextRecord, StorageConfig,
    recovery::{
        RecoveryEdge, RecoveryManifestRecord, RecoveryMemberRecord,
        restart::{RestartExpectedMember, RestartMetadata, RestartMutation},
    },
    resume_intent::ResumeIntentRecord,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::watch,
};
use workbench_authority::IngressContext;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_protocol::{
    commands::Command,
    envelope::{
        Provenance, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope,
        ServerFrame, Surface,
    },
    errors::ErrorCode,
    framing::encode_frame,
    handshake::{
        BuildInfo, Capability, ClientHello, HandshakeReply, LocalSurface, ProtocolVersion,
        ServerHello,
    },
    ids::{CapabilityName, ClientId, RequestId, RunId, SessionId},
    namespaces::lifecycle::{
        AdminId, ConfirmedGraph, GraphCas, LifecycleCommand, LifecycleQuery, LifecycleReply,
        quarantine::{QuarantineCommand, QuarantineItem, QuarantineQuery, QuarantineReply},
    },
    queries::{Query, QueryReply},
};
use workbench_web::{WebSession, WorkbenchBackend};

const TX: &str = "restart";
const MANIFEST: &str = "component";
const MAX_FRAME: u32 = 1024 * 1024;

fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}

/// A committed two-member manifest and a restart transaction, then the
/// production cold-boot transition that quarantines them.
fn seed(boot: &BootstrappedHost, config: &StorageConfig) {
    let handle = boot.storage_writer();
    let mut members = Vec::new();
    for run in ["run-a", "run-b"] {
        let record = RunContextRecord {
            run_id: run.into(),
            role: "Worker".into(),
            name: run.into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: json!([{"role":"user","content":[]}]).to_string(),
            checkpoints_json: "[]".into(),
            terminal_phase: "running".into(),
            restorable: false,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        };
        let receipt = handle.upsert_run_context_receipt(&record).unwrap();
        let payload = json!({
            "run_id":run,"incarnation":"old","parent_run_id":null,"role":"Worker",
            "active":true,"stop_reason":Value::Null,"tool_intent":[],"snapshot":receipt,
            "provider_input":{"prefix":["sent"]},"continuation":{"call_order":["first"]}
        });
        handle
            .save_resume_intent(
                ResumeIntentRecord {
                    run_id: run.into(),
                    incarnation: "old".into(),
                    payload: payload.to_string(),
                },
                None,
            )
            .unwrap();
        members.push(RecoveryMemberRecord {
            run_id: run.into(),
            incarnation: "old".into(),
            intent_revision: 0,
            snapshot: Some(receipt),
            resume: true,
            record_json:
                json!({"run_id":run,"old_incarnation":"old","parent":null,"role":"Worker"})
                    .to_string(),
        });
    }
    handle
        .prepare_recovery_manifest(RecoveryManifestRecord {
            manifest_id: MANIFEST.into(),
            schema_version: 1,
            committed: false,
            superseded: false,
            edges: vec![RecoveryEdge {
                from: "run-a".into(),
                to: "run-b".into(),
                kind: "wait".into(),
            }],
            members,
        })
        .unwrap();
    handle.commit_recovery_manifest(MANIFEST.into()).unwrap();
    let metadata = RestartMetadata {
        restart_id: TX.into(),
        capture_id: "capture".into(),
        config_identity: "config".into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: "old".into(),
        successor_build: "new".into(),
        target_identity: "target".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 71,
        old_builtin_tool_names: Default::default(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    handle
        .mutate_restart(TX.into(), None, RestartMutation::Create(Box::new(metadata)))
        .unwrap();
    let sql = rusqlite::Connection::open(&config.db_path).unwrap();
    sql.execute(
        "INSERT INTO restart_components(restart_id,manifest_id,disposition) VALUES(?1,?2,'retry_pending')",
        [TX, MANIFEST],
    )
    .unwrap();
    for run in ["run-a", "run-b"] {
        let member = RestartExpectedMember {
            run_id: run.into(),
            incarnation: "old".into(),
            root_run_id: run.into(),
            thread_id: None,
            intent_revision: 0,
            snapshot: None,
            owner_id: None,
            owner_generation: None,
            lane_id: None,
        };
        sql.execute(
            "INSERT INTO restart_expected_members(restart_id,run_id,root_run_id,member_json,manifest_id) VALUES(?1,?2,?2,?3,?4)",
            rusqlite::params![TX, run, serde_json::to_string(&member).unwrap(), MANIFEST],
        )
        .unwrap();
    }
    let (revision, _) = Database::open(config)
        .unwrap()
        .restart_revision_phase(TX)
        .unwrap()
        .unwrap();
    handle.quarantine_cold_restart(TX.into(), revision).unwrap();
}

/// One raw verified-local connection.
struct Conn {
    stream: UnixStream,
    hello: ServerHello,
}
impl Conn {
    async fn open(socket: &std::path::Path) -> Self {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let hello = ClientHello {
            supported_versions: vec![ProtocolVersion::CURRENT],
            required_capabilities: vec![
                CapabilityName::new("commands.v1").unwrap(),
                CapabilityName::new("queries.v1").unwrap(),
            ],
            optional_capabilities: Vec::new(),
            client_build: BuildInfo {
                name: "q1-install".into(),
                version: "1".into(),
                revision: None,
            },
            client_instance_id: ClientId::new("operator").unwrap(),
            desired_core: None,
            local_surface: Some(LocalSurface::Cli),
        };
        write(&mut stream, &hello).await;
        match serde_json::from_slice(&read(&mut stream).await).unwrap() {
            HandshakeReply::Accepted(hello) => Self { stream, hello },
            rejected => panic!("handshake refused: {rejected:?}"),
        }
    }
    async fn call(&mut self, request: &RequestEnvelope) -> ReplyEnvelope {
        write(&mut self.stream, request).await;
        match serde_json::from_slice(&read(&mut self.stream).await).unwrap() {
            ServerFrame::Reply(reply) => {
                assert_eq!(reply.request_id, request.request_id);
                reply
            }
            ServerFrame::Event(event) => panic!("unexpected event: {event:?}"),
        }
    }
}
async fn write(stream: &mut UnixStream, value: &impl serde::Serialize) {
    stream
        .write_all(&encode_frame(value, MAX_FRAME).unwrap())
        .await
        .unwrap();
}
async fn read(stream: &mut UnixStream) -> Vec<u8> {
    let len = stream.read_u32().await.unwrap();
    assert!(len <= MAX_FRAME);
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    bytes
}

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
        seed(&boot, &config);
        let publication = install(&Inputs::new(&boot, &backend));

        let socket = dir.sockets().join("q1-install.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, stopped) = watch::channel(false);
        let server = tokio::spawn({
            let backend = backend.clone();
            async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (backend, stopped) = (backend.clone(), stopped.clone());
                    tokio::spawn(async move {
                        let _ = unix::connection(stream, backend, stopped).await;
                    });
                }
            }
        });
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
fn error_of(reply: &ReplyEnvelope) -> (ErrorCode, &str) {
    match &reply.result {
        ReplyResult::Error(error) => (error.code, error.message.as_str()),
        other => panic!("the request was accepted: {other:?}"),
    }
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

/// Registered is not published: every Q1 request from a verified local client
/// is refused before the facade, the audit journal or the store is reached,
/// and the hello offers nothing new.
#[tokio::test]
async fn an_installed_leaf_refuses_every_request_while_management_is_off() {
    let (core, publication) = Installed::start().await;
    assert_eq!(publication, Publication::Off);
    let installed = core.boot.admin.installed_facades();
    assert!(installed.quarantine && !installed.all());
    assert!(!core.backend.hello().has(Capability::SafeRestart));

    let mut conn = Conn::open(&core.socket).await;
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
    let mut conn = Conn::open(&core.socket).await;
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

    // The verified local operator's confirmed discard goes through.
    let accepted = conn.call(&discard("op-discard", &item, epoch)).await;
    assert!(matches!(
        accepted.result,
        ReplyResult::Success(ReplyPayload::Command(_))
    ));
    core.boot.admin.wait_for_request_slots().await;
    let after = only_item(&mut conn, "r-after").await;
    assert!(after.discard_complete());
    assert!(after.revision > item.revision);
    core.shutdown().await;
}
