//! One output boundary for Unix and Web. Every reply, snapshot and event is
//! redacted here, after host processing, never by rewriting runtime/model input.
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use workbench_host::{CommandHost, projection::Projection};
use workbench_protocol::namespaces::{ConversationCommand, LifecycleQuery, LifecycleReply};
use workbench_protocol::{
    envelope::*, errors::*, events::*, handshake::*, ids::*, wire::InstanceEpoch,
};
use workbench_web::{BackendSubscription, WebSession, WorkbenchBackend};

pub struct HostBackend {
    pub host: Arc<Mutex<CommandHost>>,
    pub projection: Arc<Projection>,
    hello: ServerHello,
    redactor: Arc<secret_guard::SecretRedactor>,
    closing: AtomicBool,
    unix_clients: AtomicU32,
    web_enabled: AtomicBool,
}

pub(crate) struct ConnectedClient<'a>(&'a HostBackend);
impl Drop for ConnectedClient<'_> {
    fn drop(&mut self) {
        self.0.unix_clients.fetch_sub(1, Ordering::SeqCst);
    }
}
pub fn error(code: ErrorCode, message: &str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}

fn protect_value(
    redactor: &secret_guard::SecretRedactor,
    mut value: serde_json::Value,
) -> serde_json::Value {
    fn walk(v: &mut serde_json::Value, redactor: &secret_guard::SecretRedactor) {
        match v {
            serde_json::Value::String(s) => *s = redactor.redact(s).text,
            serde_json::Value::Array(a) => a.iter_mut().for_each(|v| walk(v, redactor)),
            serde_json::Value::Object(m) => {
                for (key, mut value) in std::mem::take(m) {
                    walk(&mut value, redactor);
                    m.insert(redactor.redact(&key).text, value);
                }
            }
            _ => (),
        }
    }
    walk(&mut value, redactor);
    value
}

fn protect<T: Serialize + DeserializeOwned>(
    redactor: &secret_guard::SecretRedactor,
    value: T,
) -> Result<T, WorkbenchError> {
    let internal = |_| error(ErrorCode::Internal, "output protection failed");
    let value = serde_json::to_value(value).map_err(internal)?;
    serde_json::from_value(protect_value(redactor, value)).map_err(internal)
}

impl HostBackend {
    pub fn new(host: Arc<Mutex<CommandHost>>, identity: String) -> Self {
        let projection = host.lock().unwrap().projection().clone();
        Self {
            host,
            projection,
            redactor: Arc::new(secret_guard::SecretRedactor::from_env()),
            closing: AtomicBool::new(false),
            unix_clients: AtomicU32::new(0),
            web_enabled: AtomicBool::new(false),
            hello: ServerHello {
                protocol_version: ProtocolVersion::CURRENT,
                capabilities: Capability::IMPLEMENTED.to_vec(),
                server_build: BuildInfo {
                    name: "evorch-core".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    revision: None,
                },
                core: CoreIdentity::new(identity).expect("sha256 identity"),
                instance_epoch: InstanceEpoch((uuid::Uuid::new_v4().as_u128() as u64).max(1)),
                limits: WireLimits::default(),
                status: CoreStatus {
                    state: CoreState::Ready,
                    retention_reasons: Vec::new(),
                    running: ActivityCounts::default(),
                },
            },
        }
    }
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }
    pub fn set_web_enabled(&self, enabled: bool) {
        self.web_enabled.store(enabled, Ordering::SeqCst);
    }
    pub(crate) fn connected(&self) -> ConnectedClient<'_> {
        self.unix_clients.fetch_add(1, Ordering::SeqCst);
        ConnectedClient(self)
    }
    fn transport_status(&self, mut status: CoreStatus) -> CoreStatus {
        status.running.clients = self.unix_clients.load(Ordering::SeqCst);
        for (retain, reason) in [
            (
                status.running.clients > 0,
                RetentionReason::ConnectedClients,
            ),
            (
                self.web_enabled.load(Ordering::SeqCst),
                RetentionReason::WebServing,
            ),
            (status.running.turns > 0, RetentionReason::ActiveTurn),
        ] {
            if retain && !status.retention_reasons.contains(&reason) {
                status.retention_reasons.push(reason);
            }
        }
        if self.closing.load(Ordering::SeqCst) {
            status.state = CoreState::Draining;
        }
        status
    }
    pub fn hello(&self) -> ServerHello {
        let mut hello = self.hello.clone();
        hello.instance_epoch = self.projection.instance_epoch();
        if let Ok(workbench_protocol::queries::QueryReply::Lifecycle(LifecycleReply::CoreStatus(
            status,
        ))) = self.host.lock().unwrap().handle_query(
            &SessionId::new("core-health").unwrap(),
            &workbench_protocol::queries::Query::Lifecycle(LifecycleQuery::GetCoreStatus),
        ) {
            hello.status = status;
        }
        hello.status = self.transport_status(hello.status);
        hello
    }
    pub fn mask<T: Serialize + DeserializeOwned>(&self, value: T) -> Result<T, WorkbenchError> {
        protect(&self.redactor, value)
    }
    pub fn request(&self, request: RequestEnvelope, provenance: Provenance) -> ReplyEnvelope {
        self.mask(self.raw_request(request, provenance))
            .unwrap_or_else(|error| ReplyEnvelope {
                request_id: RequestId::new("redacted").unwrap(),
                result: ReplyResult::Error(error),
            })
    }
    // HTTP checks the original correlation id before its final egress hook.
    fn raw_request(&self, request: RequestEnvelope, provenance: Provenance) -> ReplyEnvelope {
        let id = request.request_id.clone();
        let outcome = (|| {
            if self.closing.load(Ordering::SeqCst) {
                return Err(error(ErrorCode::Busy, "core is draining"));
            }
            request.body.validate(&self.hello.limits)?;
            let mut host = self.host.lock().unwrap();
            match request.body {
                RequestBody::Command(command) => {
                    // Reserving a NEW thread is not a claim. Existing owner rows,
                    // including released rows, remain fenced; no automatic takeover.
                    if let workbench_protocol::commands::Command::Conversation(
                        ConversationCommand::SendChat(send),
                    ) = &command
                    {
                        host.reserve_new_chat_thread(send)?;
                    }
                    host.handle_request(&request.session_id, &id, command, provenance)
                        .map(ReplyPayload::Command)
                }
                RequestBody::Query(query) => {
                    host.handle_query(&request.session_id, &query).map(|reply| {
                        ReplyPayload::Query(match reply {
                            workbench_protocol::queries::QueryReply::Lifecycle(
                                LifecycleReply::CoreStatus(status),
                            ) => workbench_protocol::queries::QueryReply::Lifecycle(
                                LifecycleReply::CoreStatus(self.transport_status(status)),
                            ),
                            reply => reply,
                        })
                    })
                }
                RequestBody::Control(Control::Ping { nonce }) => Ok(ReplyPayload::Pong { nonce }),
                RequestBody::Control(_) => Err(error(
                    ErrorCode::InvalidRequest,
                    "control requires connection scope",
                )),
            }
        })();
        let result = match outcome {
            Ok(p) => ReplyResult::Success(p),
            Err(e) => ReplyResult::Error(e),
        };
        ReplyEnvelope {
            request_id: id,
            result,
        }
    }
    pub fn subscription(
        &self,
        subscribe: &Subscribe,
    ) -> Result<workbench_host::subscriptions::Subscription, WorkbenchError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(error(ErrorCode::Busy, "core is draining"));
        }
        self.projection.subscribe_with_snapshot(subscribe)
    }
}
impl WorkbenchBackend for HostBackend {
    fn protect_output(&self, output: serde_json::Value) -> serde_json::Value {
        protect_value(&self.redactor, output)
    }

    async fn handle_request(
        &self,
        _: &WebSession,
        request: RequestEnvelope,
        provenance: Provenance,
    ) -> ReplyEnvelope {
        self.raw_request(request, provenance)
    }
    async fn subscribe(
        &self,
        _: ClientId,
        subscribe: Subscribe,
    ) -> Result<BackendSubscription, WorkbenchError> {
        let mut subscription = self.subscription(&subscribe)?;
        let started = self.mask(subscription.started.clone())?;
        let (tx, events) = tokio::sync::mpsc::channel(1);
        let redactor = self.redactor.clone();
        // Reserve capacity BEFORE reading: host queue retains a bounded terminal
        // slot and delivers ResyncRequired if this downstream is slow.
        tokio::spawn(async move {
            loop {
                let Ok(permit) = tx.reserve().await else {
                    break;
                };
                let next =
                    tokio::select! { _ = tx.closed() => break, next = subscription.recv() => next };
                let Some(event) = next else {
                    break;
                };
                let Ok(event) = protect(&redactor, event) else {
                    break;
                };
                let terminal = event.event.ends_subscription();
                permit.send(event);
                if terminal {
                    break;
                }
            }
        });
        Ok(BackendSubscription { started, events })
    }
    async fn server_hello(&self) -> Result<ServerHello, WorkbenchError> {
        Ok(self.hello())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};

    #[tokio::test]
    async fn status_tracks_connection_lifetimes_and_web_without_browser_clients() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-status".into());
        assert_eq!(backend.hello().status.running.clients, 0);
        let a = backend.connected();
        let b = backend.connected();
        backend.set_web_enabled(true);
        let status = backend.hello().status;
        assert_eq!(status.running.clients, 2);
        assert!(
            status
                .retention_reasons
                .contains(&RetentionReason::ConnectedClients)
        );
        assert!(
            status
                .retention_reasons
                .contains(&RetentionReason::WebServing)
        );
        let response = backend.request(
            RequestEnvelope {
                request_id: RequestId::new("status").unwrap(),
                session_id: SessionId::new("session").unwrap(),
                body: RequestBody::Query(workbench_protocol::queries::Query::Lifecycle(
                    LifecycleQuery::GetCoreStatus,
                )),
            },
            Provenance::human(Surface::Cli, ClientId::new("client").unwrap()),
        );
        let ReplyResult::Success(ReplyPayload::Query(
            workbench_protocol::queries::QueryReply::Lifecycle(LifecycleReply::CoreStatus(queried)),
        )) = response.result
        else {
            panic!("expected status reply")
        };
        assert_eq!(queried, status);
        drop(a);
        assert_eq!(backend.hello().status.running.clients, 1);
        drop(b);
        let status = backend.hello().status;
        assert_eq!(status.running.clients, 0);
        assert!(
            !status
                .retention_reasons
                .contains(&RetentionReason::ConnectedClients)
        );
        assert!(
            status
                .retention_reasons
                .contains(&RetentionReason::WebServing)
        );
        backend.set_web_enabled(false);
        assert!(backend.hello().status.retention_reasons.is_empty());
        backend.close();
        assert_eq!(backend.hello().status.state, CoreState::Draining);
        boot.shutdown().await.unwrap();
    }
    #[test]
    fn output_guard_covers_identifiers_and_object_keys_without_changing_numbers() {
        let canary = format!("sk-proj-{}", "B".repeat(64));
        let output = protect_value(
            &secret_guard::SecretRedactor::from_env(),
            serde_json::json!({
                canary.clone(): {"request_id":canary, "correlation_id":canary},
                "nonce":"7", "count":3, "ordinary_id":"req-1"
            }),
        );
        assert!(!output.to_string().contains("sk-proj-"), "{output}");
        assert_eq!(output["nonce"], "7");
        assert_eq!(output["count"], 3);
        assert_eq!(output["ordinary_id"], "req-1");
    }
}
