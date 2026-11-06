//! One output boundary for Unix and Web. Every reply, snapshot and event is
//! redacted here, after host processing, never by rewriting runtime/model input.
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use workbench_host::{CommandHost, projection::Projection};
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
}
pub fn error(code: ErrorCode, message: &str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}

fn protect<T: Serialize + DeserializeOwned>(
    redactor: &secret_guard::SecretRedactor,
    value: T,
) -> Result<T, WorkbenchError> {
    fn walk(v: &mut serde_json::Value, redactor: &secret_guard::SecretRedactor) {
        match v {
            serde_json::Value::String(s) => *s = redactor.redact(s).text,
            serde_json::Value::Array(a) => a.iter_mut().for_each(|v| walk(v, redactor)),
            serde_json::Value::Object(m) => m.values_mut().for_each(|v| walk(v, redactor)),
            _ => (),
        }
    }
    let internal = |_| error(ErrorCode::Internal, "output protection failed");
    let mut v = serde_json::to_value(value).map_err(internal)?;
    walk(&mut v, redactor);
    serde_json::from_value(v).map_err(internal)
}

impl HostBackend {
    pub fn new(host: Arc<Mutex<CommandHost>>, identity: String) -> Self {
        let projection = host.lock().unwrap().projection().clone();
        Self {
            host,
            projection,
            redactor: Arc::new(secret_guard::SecretRedactor::from_env()),
            closing: AtomicBool::new(false),
            hello: ServerHello {
                protocol_version: ProtocolVersion::CURRENT,
                capabilities: Capability::ALL
                    .into_iter()
                    .filter(|c| *c != Capability::SafeRestart)
                    .collect(),
                server_build: BuildInfo {
                    name: "evorch-core".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    revision: None,
                },
                core: CoreIdentity::new(identity).expect("sha256 identity"),
                instance_epoch: InstanceEpoch(uuid::Uuid::new_v4().as_u128() as u64),
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
    pub fn hello(&self) -> ServerHello {
        let mut hello = self.hello.clone();
        hello.instance_epoch = self.projection.instance_epoch();
        if let Ok(workbench_protocol::queries::QueryReply::CoreStatus(status)) =
            self.host.lock().unwrap().handle_query(
                &SessionId::new("core-health").unwrap(),
                &workbench_protocol::queries::Query::GetCoreStatus,
            )
        {
            hello.status = status;
        }
        if self.closing.load(Ordering::SeqCst) {
            hello.status.state = CoreState::Draining;
        }
        hello
    }
    pub fn mask<T: Serialize + DeserializeOwned>(&self, value: T) -> Result<T, WorkbenchError> {
        protect(&self.redactor, value)
    }
    pub fn request(&self, request: RequestEnvelope, provenance: Provenance) -> ReplyEnvelope {
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
                    if let workbench_protocol::commands::Command::SendChat(send) = &command {
                        host.reserve_new_chat_thread(send)?;
                    }
                    host.handle_request(&request.session_id, &id, command, provenance)
                        .map(ReplyPayload::Command)
                }
                RequestBody::Query(query) => host
                    .handle_query(&request.session_id, &query)
                    .map(ReplyPayload::Query),
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
        self.mask(ReplyEnvelope {
            request_id: id.clone(),
            result,
        })
        .unwrap_or_else(|e| ReplyEnvelope {
            request_id: id,
            result: ReplyResult::Error(e),
        })
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
    async fn handle_request(
        &self,
        _: &WebSession,
        request: RequestEnvelope,
        provenance: Provenance,
    ) -> ReplyEnvelope {
        self.request(request, provenance)
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
