//! One output boundary for Unix and Web. Every reply, snapshot and event is
//! redacted here, after host processing, never by rewriting runtime/model input.
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use workbench_authority::IngressContext;
use workbench_host::{CommandHost, projection::Projection};
use workbench_protocol::namespaces::LifecycleReply;
use workbench_protocol::{
    envelope::*, errors::*, events::*, handshake::*, ids::*, wire::InstanceEpoch,
};
use workbench_web::{BackendSubscription, WebSession, WorkbenchBackend};

mod endpoint;

pub struct HostBackend {
    pub host: Arc<Mutex<CommandHost>>,
    pub projection: Arc<Projection>,
    runtime: runtime::AgentRuntime,
    status: Arc<workbench_host::ingress::HostStatusProbe>,
    status_reader: Arc<StatusReader>,
    connections: Arc<workbench_host::connections::ConnectionHooks>,
    hello: ServerHello,
    redactor: Arc<secret_guard::SecretRedactor>,
    closing: AtomicBool,
    unix_clients: AtomicU32,
    web_enabled: AtomicBool,
    management_advertised: AtomicBool,
    web_clients: Mutex<Option<workbench_web::ActivityObserver>>,
    web_deliveries: workbench_web::RestartTasks,
    endpoint: endpoint::Endpoint,
    #[cfg(test)]
    pub(crate) blocking_hooks: Mutex<crate::blocking::testing::Hooks>,
}

#[derive(Default)]
struct StatusReader {
    db: Mutex<Option<storage::Database>>,
    opening: AtomicBool,
}
impl StatusReader {
    fn open(&self, runtime: &runtime::AgentRuntime) {
        if let Some(Ok(db)) = runtime.open_storage_reader() {
            *self.db.lock().unwrap() = Some(db);
        }
        self.opening.store(false, Ordering::SeqCst);
    }
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
    pub(crate) fn web_delivery_activity(&self) -> workbench_web::ActivityObserver {
        self.web_deliveries.activity()
    }
    pub(crate) fn web_delivery_busy(&self) -> bool {
        let (active, unconfirmed, _) = self.web_deliveries.activity().snapshot();
        active != 0 || unconfirmed
    }
    pub fn new(host: Arc<Mutex<CommandHost>>, identity: String) -> Self {
        let (projection, connections, runtime, status) = {
            let host = host.lock().unwrap();
            (
                host.projection().clone(),
                host.connections(),
                host.service().runtime().clone(),
                host.status_probe(),
            )
        };
        let status_reader = Arc::new(StatusReader::default());
        status_reader.open(&runtime);
        Self {
            host,
            projection,
            runtime,
            status,
            status_reader,
            connections,
            redactor: Arc::new(secret_guard::SecretRedactor::from_env()),
            closing: AtomicBool::new(false),
            unix_clients: AtomicU32::new(0),
            web_enabled: AtomicBool::new(false),
            management_advertised: AtomicBool::new(false),
            web_clients: Mutex::new(None),
            web_deliveries: Default::default(),
            endpoint: Default::default(),
            #[cfg(test)]
            blocking_hooks: Default::default(),
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
        self.endpoint.stop();
    }
    /// Publishes the current epoch and keeps the record on the latest epoch
    /// until [`Self::close`].
    pub fn publish_endpoint(&self, instance: Arc<core_instance::Instance>) -> std::io::Result<()> {
        let mut epochs = self.projection.epoch_changes();
        let epoch = *epochs.borrow_and_update();
        instance.publish(epoch)?;
        self.endpoint.start(instance, epochs, epoch);
        Ok(())
    }
    pub fn republish_endpoint(&self, instance: Arc<core_instance::Instance>, published: u64) {
        self.endpoint
            .start(instance, self.projection.epoch_changes(), published);
    }
    pub fn endpoint_published(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.endpoint.published()
    }
    pub fn set_web_enabled(&self, enabled: bool) {
        self.web_enabled.store(enabled, Ordering::SeqCst);
    }
    /// Offer `safe_restart.v1` in the hello. Only `admin_composition::capability`
    /// calls this; the capability is still withheld until the admin service's
    /// publication switch is on, and `server_hello` withholds it from non-local peers.
    pub(crate) fn advertise_management(&self) {
        self.management_advertised.store(true, Ordering::SeqCst);
    }
    fn management_published(&self) -> bool {
        self.management_advertised.load(Ordering::SeqCst) && self.status.management_enabled()
    }
    pub(crate) fn track_web_clients(&self, clients: workbench_web::ActivityObserver) {
        *self.web_clients.lock().unwrap() = Some(clients);
    }
    pub(crate) fn connected(&self) -> ConnectedClient<'_> {
        self.unix_clients.fetch_add(1, Ordering::SeqCst);
        ConnectedClient(self)
    }
    /// The handshake of `client` was accepted: its connection is open until the
    /// guard is dropped, whatever ends it, and the areas that started work for
    /// the client hear of it then.
    pub(crate) fn client_connection(
        &self,
        client: ClientId,
    ) -> workbench_host::connections::ClientConnection {
        self.connections.open(client)
    }
    /// `grace` is read by the caller: it may already hold the host lock.
    fn transport_status(
        &self,
        mut status: CoreStatus,
        grace: workbench_host::areas::browser::RendererReconnectGrace,
        invocations: usize,
    ) -> CoreStatus {
        let web = self.web_clients.lock().unwrap();
        let web_count = web.as_ref().map_or(0, |clients| {
            u32::try_from(clients.snapshot().0).unwrap_or(u32::MAX)
        });
        status.running.clients = self
            .unix_clients
            .load(Ordering::SeqCst)
            .saturating_add(web_count);
        let pending_reindex = self.pending_reindex().unwrap_or(true);
        for (retain, reason) in [
            (pending_reindex, RetentionReason::PendingReindex),
            (
                status.running.clients > 0,
                RetentionReason::ConnectedClients,
            ),
            (
                // Only legacy callers without a live connection census retain
                // an enabled listener. Production counts actual Web clients.
                web.is_none() && self.web_enabled.load(Ordering::SeqCst),
                RetentionReason::WebServing,
            ),
            (status.running.turns > 0, RetentionReason::ActiveTurn),
            (invocations > 0, RetentionReason::CliInvocation),
            (
                // Only the browser leaf's own deadline: not a client, not work.
                crate::lifecycle::renderer_grace_running(grace),
                RetentionReason::RendererReconnectGrace,
            ),
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
    // Neither the host mutex nor the shared reader: both wait behind a projection refresh.
    fn pending_reindex(&self) -> Option<bool> {
        if !self.runtime.has_run_store() {
            return Some(false);
        }
        if let Some(db) = self.status_reader.db.lock().unwrap().as_ref() {
            return db.has_pending_reindex().ok();
        }
        if !self.status_reader.opening.swap(true, Ordering::SeqCst) {
            let reader = Arc::clone(&self.status_reader);
            let runtime = self.runtime.clone();
            std::thread::spawn(move || reader.open(&runtime));
        }
        None
    }
    pub fn instance_epoch(&self) -> u64 {
        self.projection.instance_epoch().0
    }
    pub fn identity(&self) -> &str {
        self.hello.core.as_str()
    }
    pub(crate) fn wire_limits(&self) -> &WireLimits {
        &self.hello.limits
    }
    pub fn hello(&self) -> ServerHello {
        let mut hello = self.hello.clone();
        hello.instance_epoch = self.projection.instance_epoch();
        if self.management_published() {
            hello.capabilities.push(Capability::SafeRestart);
        }
        hello.status = self.transport_status(
            self.projection.cached_status(),
            self.status.renderer_reconnect_grace(),
            self.status.running_invocations(),
        );
        hello
    }
    pub fn mask<T: Serialize + DeserializeOwned>(&self, value: T) -> Result<T, WorkbenchError> {
        protect(&self.redactor, value)
    }
    /// Synchronous entry for Web and embedded callers, not Unix dispatch.
    pub fn request(
        &self,
        request: RequestEnvelope,
        provenance: impl Into<IngressContext>,
    ) -> ReplyEnvelope {
        self.mask(self.raw_request(request, provenance.into()))
            .unwrap_or_else(|error| ReplyEnvelope {
                request_id: RequestId::new("redacted").unwrap(),
                result: ReplyResult::Error(error),
            })
    }
    /// Unix owns and awaits this job before dispatching the next request.
    /// Ping has no synchronous waits and stays on the connection task.
    pub(crate) async fn request_unix(
        self: &Arc<Self>,
        request: RequestEnvelope,
        context: IngressContext,
    ) -> Result<ReplyEnvelope, tokio::task::JoinError> {
        if matches!(request.body, RequestBody::Control(Control::Ping { .. })) {
            return Ok(self.request(request, context));
        }
        self.blocking(crate::blocking::Job::Request, move |backend| {
            backend.request(request, context)
        })
        .await
    }
    // HTTP checks the original correlation id before its final egress hook.
    fn raw_request(&self, request: RequestEnvelope, context: IngressContext) -> ReplyEnvelope {
        let id = request.request_id.clone();
        let outcome = (|| {
            if let Some(action) = request.body.admin_action() {
                self.authorize_management(&context, action)?;
                return Err(error(
                    ErrorCode::UnsupportedCapability,
                    "management facade not registered",
                ));
            }
            let provenance = context.provenance().clone();
            if self.closing.load(Ordering::SeqCst) {
                return Err(error(ErrorCode::Busy, "core is draining"));
            }
            request.body.validate(&self.hello.limits)?;
            if let RequestBody::Control(Control::Ping { nonce }) = &request.body {
                return Ok(ReplyPayload::Pong { nonce: *nonce });
            }
            let mut host = self.host.lock().unwrap();
            match request.body {
                RequestBody::Command(command) => {
                    // Reserving a NEW thread is not a claim. Existing owner rows,
                    // including released rows, remain fenced; no automatic takeover.
                    host.handle_request(&request.session_id, &id, command, provenance)
                        .map(ReplyPayload::Command)
                }
                RequestBody::Query(query) => {
                    // The transport's provenance, as for commands: never a surface
                    // or owner the request body names.
                    host.handle_query(&request.session_id, &query, &provenance)
                        .map(|reply| {
                            ReplyPayload::Query(match reply {
                                workbench_protocol::queries::QueryReply::Lifecycle(
                                    LifecycleReply::CoreStatus(status),
                                ) => workbench_protocol::queries::QueryReply::Lifecycle(
                                    LifecycleReply::CoreStatus(self.transport_status(
                                        status,
                                        host.renderer_reconnect_grace(),
                                        host.service().restart_gate().running_invocations(),
                                    )),
                                ),
                                reply => reply,
                            })
                        })
                }
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
        context: &IngressContext,
    ) -> Result<workbench_host::subscriptions::Subscription, WorkbenchError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(error(ErrorCode::Busy, "core is draining"));
        }
        if subscribe.scope == workbench_protocol::events::SubscriptionScope::Management {
            self.authorize_management(
                context,
                workbench_protocol::namespaces::lifecycle::AdminAction::Status,
            )?;
            // The management stream is the core-status stream: it carries the
            // core state changes (including `Restarting`) and no thread, run or
            // project data, behind the same authorization as every management read.
        }
        self.projection.subscribe_with_snapshot(subscribe)
    }
}
impl WorkbenchBackend for HostBackend {
    fn authorize_management(
        &self,
        context: &IngressContext,
        action: workbench_protocol::namespaces::lifecycle::AdminAction,
    ) -> Result<(), WorkbenchError> {
        if let Some(admin) = self.host.lock().unwrap().admin() {
            return admin.authorize(context, action);
        }
        context.authorize().map(|_| ()).map_err(|_| {
            error(
                ErrorCode::PermissionDenied,
                "verified human management authority required",
            )
        })
    }
    async fn drain_for_restart(&self) -> Result<(), WorkbenchError> {
        self.web_deliveries
            .drain()
            .await
            .map_err(|_| error(ErrorCode::Internal, "web delivery drain unconfirmed"))
    }
    fn protect_output(&self, output: serde_json::Value) -> serde_json::Value {
        protect_value(&self.redactor, output)
    }

    async fn handle_request(
        &self,
        _: &WebSession,
        request: RequestEnvelope,
        provenance: IngressContext,
    ) -> ReplyEnvelope {
        if request.body.admin_action().is_none() {
            return self.raw_request(request, provenance);
        }
        let request_id = request.request_id.clone();
        let result=async {
            let action=request.body.admin_action().unwrap();
            self.authorize_management(&provenance,action)?;
            request.body.validate(&self.hello.limits)?;
            let admin={self.host.lock().unwrap().admin()}.ok_or_else(||error(ErrorCode::UnsupportedCapability,"management facade not registered"))?;
            match request.body {
                RequestBody::Command(workbench_protocol::commands::Command::Lifecycle(intent))=>{
                    let authorized=admin.authorize_request(provenance,intent)?;
                    let accepted=admin.submit(authorized)?.await.map_err(|_|error(ErrorCode::Internal,"management result unknown"))??;
                    Ok(ReplyPayload::Command(workbench_protocol::commands::CommandReply::with_result(workbench_protocol::commands::CommandResult::Lifecycle(workbench_protocol::namespaces::lifecycle::LifecycleResult::Accepted(accepted.into_receipt())))))
                },
                RequestBody::Query(workbench_protocol::queries::Query::Lifecycle(query))=>{
                    let authorized=admin.authorize_query(provenance,query)?;
                    let view=admin.query(authorized)?.await.map_err(|_|error(ErrorCode::Internal,"management view unavailable"))??;
                    view.validate(&self.hello.limits)?;
                    Ok(ReplyPayload::Query(workbench_protocol::queries::QueryReply::Lifecycle(view)))
                },
                _=>Err(error(ErrorCode::InvalidRequest,"unknown management request")),
            }
        }.await;
        ReplyEnvelope {
            request_id,
            result: match result {
                Ok(reply) => ReplyResult::Success(reply),
                Err(error) => ReplyResult::Error(error),
            },
        }
    }
    async fn subscribe(
        &self,
        _: ClientId,
        subscribe: Subscribe,
        context: IngressContext,
    ) -> Result<BackendSubscription, WorkbenchError> {
        let mut subscription = self.subscription(&subscribe, &context)?;
        let started = self.mask(subscription.started.clone())?;
        let (tx, events) = tokio::sync::mpsc::channel(1);
        let redactor = self.redactor.clone();
        // Reserve capacity BEFORE reading: host queue retains a bounded terminal
        // slot and delivers ResyncRequired if this downstream is slow.
        self.web_deliveries.spawn(async move {
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
        }).map_err(|_| error(ErrorCode::Draining, "web delivery admission closed"))?;
        Ok(BackendSubscription { started, events })
    }
    async fn server_hello(&self, context: &IngressContext) -> Result<ServerHello, WorkbenchError> {
        let mut hello = self.hello();
        if !context.authority().is_verified_local() {
            hello.capabilities.retain(|c| c.name() != "safe_restart.v1");
        }
        Ok(hello)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
    use workbench_protocol::namespaces::LifecycleQuery;

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
    #[tokio::test]
    async fn hello_waits_for_neither_the_host_lock_nor_the_shared_reader() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-hello".into());
        let expected = backend.hello();
        let host = backend.host.lock().unwrap();
        let hello = backend
            .runtime
            .with_storage_reader(|_| {
                Ok(std::thread::scope(|scope| {
                    scope.spawn(|| backend.hello()).join().unwrap()
                }))
            })
            .unwrap()
            .unwrap();
        drop(host);
        assert_eq!(hello.status, expected.status);
        assert_eq!(hello.instance_epoch, expected.instance_epoch);
        assert_eq!(hello.capabilities, expected.capabilities);
        boot.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn hello_without_its_own_reader_retains_instead_of_using_the_shared_reader() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-hello".into());
        assert!(
            !backend
                .hello()
                .status
                .retention_reasons
                .contains(&RetentionReason::PendingReindex)
        );
        *backend.status_reader.db.lock().unwrap() = None;
        backend.status_reader.opening.store(true, Ordering::SeqCst);
        let hello = backend
            .runtime
            .with_storage_reader(|_| {
                Ok(std::thread::scope(|scope| {
                    scope.spawn(|| backend.hello()).join().unwrap()
                }))
            })
            .unwrap()
            .unwrap();
        assert!(
            hello
                .status
                .retention_reasons
                .contains(&RetentionReason::PendingReindex)
        );
        backend.status_reader.open(&backend.runtime);
        assert!(
            !backend
                .hello()
                .status
                .retention_reasons
                .contains(&RetentionReason::PendingReindex)
        );
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
