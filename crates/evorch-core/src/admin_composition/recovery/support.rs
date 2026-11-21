//! Shared by the leaf tests in this tree: a committed cold graph, one raw
//! verified-local Unix connection, and the real `unix::connection` server.
//! Nothing is faked; synchronization is by replies and joined request slots.
use crate::{backend::HostBackend, unix};
use serde_json::{Value, json};
use std::sync::Arc;
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
use workbench_host::bootstrap::BootstrappedHost;
use workbench_protocol::{
    envelope::{ReplyEnvelope, ReplyResult, RequestEnvelope, ServerFrame},
    errors::ErrorCode,
    framing::encode_frame,
    handshake::{
        BuildInfo, ClientHello, HandshakeReply, LocalSurface, ProtocolVersion, ServerHello,
    },
    ids::{CapabilityName, ClientId},
    namespaces::lifecycle::AdminId,
};

pub(super) const TX: &str = "restart";
pub(super) const MANIFEST: &str = "component";
const MAX_FRAME: u32 = 1024 * 1024;

pub(super) fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}

/// A committed two-member manifest and a restart transaction, then the
/// production cold-boot transition that quarantines them.
pub(super) fn seed(boot: &BootstrappedHost, config: &StorageConfig, threads: bool) {
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
            thread_id: threads.then(|| run.into()),
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
pub(super) struct Conn {
    stream: UnixStream,
    pub(super) hello: ServerHello,
}
impl Conn {
    pub(super) async fn open(socket: &std::path::Path, name: &str) -> Self {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let hello = ClientHello {
            supported_versions: vec![ProtocolVersion::CURRENT],
            required_capabilities: vec![
                CapabilityName::new("commands.v1").unwrap(),
                CapabilityName::new("queries.v1").unwrap(),
            ],
            optional_capabilities: Vec::new(),
            client_build: BuildInfo {
                name: name.into(),
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
    pub(super) async fn call(&mut self, request: &RequestEnvelope) -> ReplyEnvelope {
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

/// Serve `backend` on `listener` with the real `unix::connection`.
pub(super) fn serve(
    listener: tokio::net::UnixListener,
    backend: Arc<HostBackend>,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop, stopped) = watch::channel(false);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = unix::connection(stream, backend, stopped).await;
            });
        }
    });
    (stop, server)
}

pub(super) fn error_of(reply: &ReplyEnvelope) -> (ErrorCode, &str) {
    match &reply.result {
        ReplyResult::Error(error) => (error.code, error.message.as_str()),
        other => panic!("the request was accepted: {other:?}"),
    }
}
