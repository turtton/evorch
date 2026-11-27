//! The `auth` area through the production command path: the restart contract
//! of a login (R1), the single use of a callback, and that no secret leaves
//! the credential store.
mod common;

use sandbox::{CredentialError, CredentialStore, Secret};
use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Notify, oneshot};
use workbench_host::{
    CommandHost,
    areas::{
        AreaTasks, Areas,
        auth::{AuthArea, AuthParts, BoxFuture, Callback, LoginFailure, LoginFlow, LoginStarter},
    },
    bootstrap::restart::HostRestartPhase,
    commands::CommandService,
};
use workbench_protocol::{
    WorkbenchError,
    commands::{Command, CommandDisposition, CommandResult},
    errors::ErrorCode,
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::{OperationId, RequestId, SessionId},
    namespaces::auth::*,
    operation::{CancelReason, OperationState},
    queries::{Query, QueryReply},
};

const SENTINEL: &str = "SENTINEL-d41d8cd98f00b204";
const STATE: &str = "state-1";
/// The key the fake provider refuses.
const REFUSED_KEY: &str = "refused-cursor-key";
const CONFIG: &str = "version = 3\n\
[providers.sub]\n\
type = 'anthropic-subscription'\n\
credential = { type = 'keyring', service = 'evorch', account = 'claude-acct' }\n\
models = ['claude-test']\n\
default_model = 'claude-test'\n\
[providers.cur]\n\
type = 'cursor'\n\
credential = { type = 'keyring', service = 'evorch', account = 'cursor-acct' }\n\
models = ['cursor-m']\n\
default_model = 'cursor-m'\n\
[providers.key]\n\
type = 'openai-compatible'\n\
base_url = 'https://llm.example.invalid/v1'\n\
credential = { type = 'keyring', service = 'evorch', account = 'key-acct' }\n\
models = ['m']\n\
default_model = 'm'\n\
[providers.envkey]\n\
type = 'openai-compatible'\n\
base_url = 'https://llm.example.invalid/v1'\n\
api_key_env = 'EVORCH_TEST_ABSENT_KEY'\n\
models = ['m']\n\
default_model = 'm'\n";

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

/// A credential store in memory whose calls can be held, as a keychain that
/// waits on a locked session would.
#[derive(Default)]
struct MemoryStore {
    values: Mutex<HashMap<String, String>>,
    held: Mutex<bool>,
    released: Condvar,
    entered: Notify,
}

impl MemoryStore {
    fn hold(&self) {
        *self.held.lock().unwrap() = true;
    }

    fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.released.notify_all();
    }

    fn pass(&self) {
        self.entered.notify_one();
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.released.wait(held).unwrap();
        }
    }

    fn value(&self, key: &str) -> Option<String> {
        self.values.lock().unwrap().get(key).cloned()
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        self.pass();
        Ok(self.value(key).map(Secret::from))
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        self.pass();
        self.values
            .lock()
            .unwrap()
            .insert(key.to_owned(), value.expose().to_owned());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        self.pass();
        self.values.lock().unwrap().remove(key);
        Ok(())
    }
}

/// What the fake provider observed, and the levers a test pulls.
#[derive(Default)]
struct Probe {
    completed: Mutex<Vec<String>>,
    entered: Notify,
    release: Notify,
    hold_complete: AtomicBool,
    loopback: Mutex<Option<oneshot::Sender<String>>>,
    begun: AtomicUsize,
    released: AtomicUsize,
    /// Holds `begin` (as a port bind or a slow provider would).
    begin_held: Mutex<bool>,
    begin_released: Condvar,
    begin_entered: Notify,
}

impl Probe {
    fn hold_begin(&self) {
        *self.begin_held.lock().unwrap() = true;
    }

    fn release_begin(&self) {
        *self.begin_held.lock().unwrap() = false;
        self.begin_released.notify_all();
    }
}

struct FakeFlow {
    probe: Arc<Probe>,
    store: Arc<MemoryStore>,
    account: String,
    loopback: Mutex<Option<oneshot::Receiver<String>>>,
}

impl LoginFlow for FakeFlow {
    fn authorize_url(&self) -> &str {
        "https://login.example.invalid/authorize?state=state-1"
    }

    fn accepts_input(&self) -> bool {
        true
    }

    fn check_input(&self, input: &str) -> Result<Callback, LoginInputRejection> {
        match input.split_once('#') {
            None => Err(LoginInputRejection::Malformed),
            Some((_, state)) if state != STATE => Err(LoginInputRejection::StateMismatch),
            Some((code, _)) => Ok(Callback::new(code)),
        }
    }

    fn wait_loopback(
        &self,
        _stop: Arc<AtomicBool>,
    ) -> BoxFuture<'_, Result<Callback, LoginFailure>> {
        let receiver = self.loopback.lock().unwrap().take();
        Box::pin(async move {
            match receiver {
                Some(receiver) => match receiver.await {
                    Ok(code) => Ok(Callback::new(code)),
                    Err(_) => std::future::pending().await,
                },
                None => std::future::pending().await,
            }
        })
    }

    fn release(&self) -> BoxFuture<'_, ()> {
        self.probe.released.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn complete(&self, callback: Callback) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>> {
        Box::pin(async move {
            self.probe
                .completed
                .lock()
                .unwrap()
                .push(callback.expose().to_owned());
            self.probe.entered.notify_one();
            if self.probe.hold_complete.load(Ordering::SeqCst) {
                self.probe.release.notified().await;
            }
            self.store
                .set(
                    &self.account,
                    &Secret::from(format!("token-of-{}", callback.expose())),
                )
                .map_err(|_| LoginFailure::StoreUnavailable)?;
            Ok(Some(1_900_000_000))
        })
    }
}

struct FakeStarter {
    probe: Arc<Probe>,
    store: Arc<MemoryStore>,
}

impl LoginStarter for FakeStarter {
    fn exchange_api_key(
        &self,
        method: LoginMethod,
        account: &str,
        key: Secret,
    ) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>> {
        let account = account.to_owned();
        Box::pin(async move {
            if method != LoginMethod::Cursor || key.expose() == REFUSED_KEY {
                return Err(LoginFailure::Rejected);
            }
            self.store
                .set(&account, &Secret::from("exchanged-session".to_owned()))
                .map_err(|_| LoginFailure::StoreUnavailable)?;
            Ok(Some(1_900_000_000))
        })
    }

    fn begin(
        &self,
        _method: LoginMethod,
        account: &str,
    ) -> Result<Arc<dyn LoginFlow>, LoginFailure> {
        self.probe.begun.fetch_add(1, Ordering::SeqCst);
        self.probe.begin_entered.notify_one();
        let mut held = self.probe.begin_held.lock().unwrap();
        while *held {
            held = self.probe.begin_released.wait(held).unwrap();
        }
        drop(held);
        let (sender, receiver) = oneshot::channel();
        *self.probe.loopback.lock().unwrap() = Some(sender);
        Ok(Arc::new(FakeFlow {
            probe: self.probe.clone(),
            store: self.store.clone(),
            account: account.to_owned(),
            loopback: Mutex::new(Some(receiver)),
        }))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    host: CommandHost,
    auth: AuthArea,
    store: Arc<MemoryStore>,
    probe: Arc<Probe>,
    events: workbench_host::subscriptions::Subscription,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), CONFIG).unwrap();
        let store = Arc::new(MemoryStore::default());
        let probe = Arc::new(Probe::default());
        let service = CommandService::new(common::runtime(), tokio::runtime::Handle::current());
        let host = CommandHost::new(service);
        let source = host.projection().source("auth").unwrap();
        let auth = AuthArea::with_parts(AuthParts {
            config_dir: dir.path().to_path_buf(),
            startup: Arc::new(config::Config::default()),
            credentials: store.clone(),
            tasks: AreaTasks::new(tokio::runtime::Handle::current()),
            source,
            starter: Arc::new(FakeStarter {
                probe: probe.clone(),
                store: store.clone(),
            }),
            login_timeout: std::time::Duration::from_secs(3600),
        });
        let host = host.with_areas(Areas {
            auth: auth.clone(),
            ..Default::default()
        });
        let events = host
            .projection()
            .subscribe_with_snapshot(&Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .unwrap();
        Self {
            _dir: dir,
            host,
            auth,
            store,
            probe,
            events,
        }
    }

    fn run(&mut self, command: AuthCommand) -> Result<CommandResult, WorkbenchError> {
        self.host
            .handle_command(Command::Auth(command), common::web())
            .map(|reply| *reply.result.expect("a result"))
    }

    /// Starts the login and returns once its session exists: the core announces
    /// that with `login_changed`, and the first section of the login (reading the
    /// profile, preparing the flow) has run by then.
    async fn start_login(&mut self, id: &str) {
        match self
            .run(AuthCommand::StartLogin(StartLogin {
                operation_id: op(id),
                profile: "sub".into(),
            }))
            .unwrap()
        {
            CommandResult::Auth(AuthResult::Operation(view)) => {
                assert_eq!(view.operation.state, OperationState::Pending);
                assert_eq!(view.kind, AuthOperationKind::Login);
            }
            other => panic!("{other:?}"),
        }
        loop {
            let envelope = self.events.recv().await.expect("the subscription is open");
            if let UiEvent::Auth(AuthEvent::LoginChanged { login_id }) = envelope.event
                && login_id == op(id)
            {
                return;
            }
        }
    }

    fn submit(&mut self, id: &str, text: &str) -> Result<LoginInputOutcome, WorkbenchError> {
        self.run(AuthCommand::SubmitLoginInput(SubmitLoginInput {
            login_id: op(id),
            input: SecretValue::new(text),
        }))
        .map(|result| match result {
            CommandResult::Auth(AuthResult::LoginInput(outcome)) => outcome,
            other => panic!("{other:?}"),
        })
    }

    fn paste(&mut self, id: &str, text: &str) -> LoginInputOutcome {
        self.submit(id, text).unwrap()
    }

    fn rejected(&mut self, id: &str, text: &str) -> LoginInputRejection {
        match self.paste(id, text) {
            LoginInputOutcome::Rejected { reason } => reason,
            LoginInputOutcome::Accepted => panic!("accepted {text:?}"),
        }
    }

    fn query(&self, query: AuthQuery) -> AuthReply {
        match self
            .host
            .handle_query(
                &SessionId::new("session").unwrap(),
                &Query::Auth(query),
                &workbench_host::provenance::local_desktop(),
            )
            .unwrap()
        {
            QueryReply::Auth(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    fn login(&self, id: &str) -> LoginView {
        match self.query(AuthQuery::GetLogin { login_id: op(id) }) {
            AuthReply::Login(view) => view,
            other => panic!("{other:?}"),
        }
    }

    async fn settled(&self, id: &str) -> AuthOperationView {
        self.auth.wait_operation(&op(id)).await.unwrap()
    }

    fn gate(&self) -> workbench_host::bootstrap::restart::HostRestartGate {
        self.host.service().restart_gate()
    }
}

fn cancelled(reason: CancelReason) -> OperationState {
    OperationState::Cancelled { reason }
}

fn save_key(id: &str, profile: &str, value: &str) -> AuthCommand {
    AuthCommand::SaveApiKey(SaveApiKey {
        operation_id: op(id),
        profile: profile.into(),
        value: SecretValue::new(value),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_planned_restart_during_the_login_wait_seals_without_waiting_for_the_login() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    let gate = f.gate();
    // The wait for the person holds no restart permit, so admission drains
    // while the login still waits for an answer.
    gate.drained().await;
    assert_eq!(f.login("login-1").phase, LoginPhase::Waiting);

    assert!(gate.transition(1, HostRestartPhase::Preparing));
    assert!(gate.transition(1, HostRestartPhase::Sealing));
    gate.drained().await;

    let settled = f.settled("login-1").await;
    assert_eq!(settled.operation.state, cancelled(CancelReason::Restart));
    let view = f.login("login-1");
    assert_eq!(view.phase, LoginPhase::Settled);
    assert!(view.authorize_url.is_none() && !view.accepts_input);
    assert!(f.probe.completed.lock().unwrap().is_empty());
    assert_eq!(f.store.value("claude-acct"), None);
    // A late answer meets the closed gate; nothing revives the login.
    assert!(f.submit("login-1", "late#state-1").is_err());
    assert!(f.probe.completed.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_during_the_login_wait_settles_it_as_cancelled_shutdown() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    let gate = f.gate();
    gate.drained().await;
    assert!(gate.transition(1, HostRestartPhase::ShuttingDown));
    let settled = f.settled("login-1").await;
    assert_eq!(settled.operation.state, cancelled(CancelReason::Shutdown));
    assert_eq!(f.store.value("claude-acct"), None);
}

#[tokio::test]
async fn a_callback_that_meets_a_closed_gate_writes_nothing() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    // Single-threaded: the login task has not run since the command, so the
    // callback and the restart arrive together.
    assert_eq!(
        f.paste("login-1", "code#state-1"),
        LoginInputOutcome::Accepted
    );
    assert!(f.gate().transition(1, HostRestartPhase::Preparing));
    let settled = f.settled("login-1").await;
    assert_eq!(settled.operation.state, cancelled(CancelReason::Restart));
    assert!(f.probe.completed.lock().unwrap().is_empty());
    assert_eq!(f.store.value("claude-acct"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_input_is_checked_against_the_login_and_used_once() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    assert!(f.login("login-1").accepts_input);
    assert!(f.login("login-1").authorize_url.is_some());

    assert_eq!(
        f.rejected("login-1", "no-state-at-all"),
        LoginInputRejection::Malformed
    );
    assert_eq!(
        f.rejected("login-1", "code#other-state"),
        LoginInputRejection::StateMismatch
    );
    assert_eq!(
        f.rejected("nobody", "code#state-1"),
        LoginInputRejection::UnknownLogin
    );
    // Refusals consume nothing.
    assert_eq!(f.login("login-1").phase, LoginPhase::Waiting);

    f.probe.hold_complete.store(true, Ordering::SeqCst);
    assert_eq!(
        f.paste("login-1", "first#state-1"),
        LoginInputOutcome::Accepted
    );
    f.probe.entered.notified().await;
    assert_eq!(
        f.rejected("login-1", "second#state-1"),
        LoginInputRejection::AlreadyUsed
    );
    f.probe.release.notify_one();

    let settled = f.settled("login-1").await;
    assert_eq!(settled.operation.state, OperationState::Ready);
    assert_eq!(*f.probe.completed.lock().unwrap(), vec!["first".to_owned()]);
    assert_eq!(
        f.store.value("claude-acct").as_deref(),
        Some("token-of-first")
    );
    assert!(matches!(
        f.rejected("login-1", "third#state-1"),
        LoginInputRejection::AlreadyUsed | LoginInputRejection::NotWaiting
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn of_the_loopback_and_a_paste_only_the_first_is_committed() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    f.probe.hold_complete.store(true, Ordering::SeqCst);
    let loopback = f.probe.loopback.lock().unwrap().take().unwrap();
    loopback.send("from-loopback".into()).unwrap();
    f.probe.entered.notified().await;
    // The loopback callback is being exchanged: a paste now is a second use.
    assert_eq!(
        f.rejected("login-1", "from-paste#state-1"),
        LoginInputRejection::AlreadyUsed
    );
    f.probe.release.notify_one();
    assert_eq!(
        f.settled("login-1").await.operation.state,
        OperationState::Ready
    );
    assert_eq!(
        *f.probe.completed.lock().unwrap(),
        vec!["from-loopback".to_owned()]
    );
    assert_eq!(
        f.store.value("claude-acct").as_deref(),
        Some("token-of-from-loopback")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_login_settles_it_and_frees_the_profile() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    let other = f.run(AuthCommand::StartLogin(StartLogin {
        operation_id: op("login-2"),
        profile: "sub".into(),
    }));
    assert_eq!(other.unwrap_err().code, ErrorCode::Conflict);
    // The same request again is the same login: nothing is started twice.
    let again = f
        .host
        .handle_command(
            Command::Auth(AuthCommand::StartLogin(StartLogin {
                operation_id: op("login-1"),
                profile: "sub".into(),
            })),
            common::web(),
        )
        .unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(f.probe.begun.load(Ordering::SeqCst), 1);

    // The cancel is accepted at once; the login task settles it once the
    // flow gave back what it held.
    match f
        .run(AuthCommand::CancelLogin(CancelLogin {
            login_id: op("login-1"),
        }))
        .unwrap()
    {
        CommandResult::Auth(AuthResult::Operation(view)) => {
            assert_eq!(view.kind, AuthOperationKind::Login);
        }
        other => panic!("{other:?}"),
    }
    let settled = f.settled("login-1").await;
    assert_eq!(settled.operation.state, cancelled(CancelReason::Requested));
    assert_eq!(f.login("login-1").phase, LoginPhase::Settled);
    assert_eq!(
        f.probe.released.load(Ordering::SeqCst),
        1,
        "the flow was released before the outcome"
    );
    f.start_login("login-3").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_is_saved_deleted_and_never_run_twice_for_one_operation_id() {
    let mut f = Fixture::new();
    f.run(save_key("save-1", "key", "first-value")).unwrap();
    assert_eq!(
        f.settled("save-1").await.operation.state,
        OperationState::Ready
    );
    assert_eq!(f.store.value("key-acct").as_deref(), Some("first-value"));

    // The same id with another value is the same operation: it does not run.
    let reply = f
        .host
        .handle_command(
            Command::Auth(save_key("save-1", "key", "second-value")),
            common::web(),
        )
        .unwrap();
    assert_eq!(reply.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(f.store.value("key-acct").as_deref(), Some("first-value"));
    // Another profile under the same id is a different request.
    assert_eq!(
        f.run(save_key("save-1", "envkey", "x")).unwrap_err().code,
        ErrorCode::Conflict
    );

    for (id, profile, code) in [
        ("save-2", "envkey", ErrorCode::InvalidRequest),
        ("save-3", "sub", ErrorCode::InvalidRequest),
        ("save-4", "missing", ErrorCode::NotFound),
    ] {
        f.run(save_key(id, profile, "x")).unwrap();
        assert_eq!(
            f.settled(id).await.operation.state,
            OperationState::Failed { code },
            "{profile}"
        );
    }

    f.run(delete("delete-1", "key", "key-acct", &[])).unwrap();
    assert_eq!(
        f.settled("delete-1").await.operation.state,
        OperationState::Ready
    );
    assert_eq!(f.store.value("key-acct"), None);

    f.run(AuthCommand::RefreshCredentials(RefreshCredentials {
        operation_id: op("refresh-1"),
    }))
    .unwrap();
    f.settled("refresh-1").await;
    match f.query(AuthQuery::GetCredentialStatus {}) {
        AuthReply::Status(status) => {
            let slot = |name: &str| status.slots.iter().find(|s| s.profile == name).unwrap();
            assert_eq!(slot("key").state, CredentialState::Missing);
            assert_eq!(slot("key").login, None);
            assert_eq!(slot("sub").login, Some(LoginMethod::Claude));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_secret_leaves_the_credential_store() {
    let mut f = Fixture::new();
    let session = SessionId::new("session").unwrap();
    let mut seen: Vec<(&str, String)> = Vec::new();

    f.start_login("login-1").await;
    let request = RequestId::new("request-1").unwrap();
    let reply = f
        .host
        .handle_request(
            &session,
            &request,
            Command::Auth(AuthCommand::SubmitLoginInput(SubmitLoginInput {
                login_id: op("login-1"),
                input: SecretValue::new(format!("{SENTINEL}-code#state-1")),
            })),
            common::web(),
        )
        .unwrap();
    seen.push((
        "paste reply",
        format!("{reply:?} {}", serde_json::to_string(&reply).unwrap()),
    ));
    seen.push((
        "paste receipt",
        format!("{:?}", f.host.command_receipt(&session, &request)),
    ));
    f.settled("login-1").await;

    let key_request = RequestId::new("request-2").unwrap();
    let reply = f
        .host
        .handle_request(
            &session,
            &key_request,
            Command::Auth(save_key("save-1", "key", &format!("{SENTINEL}-key"))),
            common::web(),
        )
        .unwrap();
    seen.push(("save reply", serde_json::to_string(&reply).unwrap()));
    seen.push((
        "save receipt",
        format!("{:?}", f.host.command_receipt(&session, &key_request)),
    ));
    f.settled("save-1").await;

    for query in [
        AuthQuery::GetCredentialStatus {},
        AuthQuery::GetLogin {
            login_id: op("login-1"),
        },
        AuthQuery::GetAuthOperation {
            operation_id: op("login-1"),
        },
        AuthQuery::GetAuthOperation {
            operation_id: op("save-1"),
        },
        AuthQuery::ListAuthOperations {},
    ] {
        seen.push(("query", serde_json::to_string(&f.query(query)).unwrap()));
    }
    // Every event up to the end of the save.
    loop {
        let envelope = f.events.recv().await.expect("the subscription is open");
        seen.push(("event", serde_json::to_string(&envelope).unwrap()));
        if matches!(
            &envelope.event,
            UiEvent::Auth(AuthEvent::AuthOperationCompleted { operation })
                if operation.operation.operation_id == op("save-1")
        ) {
            break;
        }
    }
    // The secrets did reach the store, and only the store.
    assert!(f.store.value("claude-acct").unwrap().contains(SENTINEL));
    assert!(f.store.value("key-acct").unwrap().contains(SENTINEL));
    for (label, text) in seen {
        assert!(!text.contains(SENTINEL), "{label} carries a secret: {text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_credential_store_does_not_stop_the_other_clients() {
    let mut f = Fixture::new();
    f.start_login("login-1").await;
    f.store.hold();
    f.run(save_key("save-1", "key", "value")).unwrap();
    f.store.entered.notified().await;

    // The save is stuck in the store; everything below still answers.
    assert_eq!(f.login("login-1").phase, LoginPhase::Waiting);
    assert!(matches!(
        f.query(AuthQuery::GetAuthOperation {
            operation_id: op("save-1")
        }),
        AuthReply::Operation(view) if view.operation.state == OperationState::Pending
    ));
    let AuthReply::Operations { operations } = f.query(AuthQuery::ListAuthOperations {}) else {
        panic!("operations")
    };
    assert_eq!(operations.len(), 2);
    assert!(matches!(
        f.run(AuthCommand::CancelLogin(CancelLogin {
            login_id: op("login-1")
        }))
        .unwrap(),
        CommandResult::Auth(AuthResult::Operation(_))
    ));

    f.store.release();
    assert_eq!(
        f.settled("save-1").await.operation.state,
        OperationState::Ready
    );
}

/// The handler of `start_login` looks at memory only: preparing the flow (a
/// port bound, a provider reached) runs on the operation task, and the permit
/// the command was accepted with is kept until the login waits for the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_login_preparation_does_not_block_the_handler_and_keeps_the_permit() {
    let mut f = Fixture::new();
    f.probe.hold_begin();
    // Returns while `begin` is still held: were it called by the handler,
    // this would never return.
    f.run(AuthCommand::StartLogin(StartLogin {
        operation_id: op("login-1"),
        profile: "sub".into(),
    }))
    .unwrap();
    f.probe.begin_entered.notified().await;
    let again = f.run(AuthCommand::StartLogin(StartLogin {
        operation_id: op("login-2"),
        profile: "sub".into(),
    }));
    assert_eq!(
        again.unwrap_err().code,
        ErrorCode::Conflict,
        "a login being prepared already holds its profile"
    );
    // The session does not exist yet, and the permit is still held: a planned
    // restart cannot drain while the preparation runs.
    assert!(matches!(
        f.host.handle_query(
            &SessionId::new("session").unwrap(),
            &Query::Auth(AuthQuery::GetLogin { login_id: op("login-1") }), &workbench_host::provenance::local_desktop()),
        Err(error) if error.code == ErrorCode::NotFound
    ));
    f.probe.release_begin();
    loop {
        let envelope = f.events.recv().await.unwrap();
        if let UiEvent::Auth(AuthEvent::LoginChanged { login_id }) = envelope.event
            && login_id == op("login-1")
        {
            break;
        }
    }
    assert_eq!(f.login("login-1").phase, LoginPhase::Waiting);
    // Waiting for the person holds no permit any more.
    f.gate().drained().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_the_preparation_ends_the_login_without_waiting_for_the_person() {
    let mut f = Fixture::new();
    f.probe.hold_begin();
    f.run(AuthCommand::StartLogin(StartLogin {
        operation_id: op("login-1"),
        profile: "sub".into(),
    }))
    .unwrap();
    f.probe.begin_entered.notified().await;
    f.run(AuthCommand::CancelLogin(CancelLogin {
        login_id: op("login-1"),
    }))
    .unwrap();
    f.probe.release_begin();
    let settled = f.settled("login-1").await;
    assert_eq!(settled.operation.state, cancelled(CancelReason::Requested));
    assert_eq!(f.probe.released.load(Ordering::SeqCst), 1);
    assert_eq!(f.login("login-1").phase, LoginPhase::Settled);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_for_a_profile_without_a_login_fails_as_an_operation() {
    let mut f = Fixture::new();
    for (id, profile, code) in [
        ("login-1", "envkey", ErrorCode::InvalidRequest),
        ("login-2", "missing", ErrorCode::NotFound),
    ] {
        f.run(AuthCommand::StartLogin(StartLogin {
            operation_id: op(id),
            profile: profile.into(),
        }))
        .unwrap();
        let settled = f.settled(id).await;
        assert_eq!(settled.operation.state, OperationState::Failed { code });
    }
    assert_eq!(f.probe.begun.load(Ordering::SeqCst), 0);
}

/// Cursor also signs in with a dashboard API key: the core hands the key to the
/// provider's own exchange and stores the result as a login is stored (the real
/// bundle keeps the key to renew the access token); the typed key is not shown in
/// any result a client reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_api_key_is_exchanged_and_stored_as_a_login() {
    let mut f = Fixture::new();
    let typed = "typed-cursor-dashboard-key";
    f.run(save_key("cursor-1", "cur", typed)).unwrap();
    let settled = f.settled("cursor-1").await;
    assert_eq!(settled.operation.state, OperationState::Ready);
    assert_eq!(
        f.store.value("cursor-acct").as_deref(),
        Some("exchanged-session")
    );
    assert!(!format!("{settled:?}").contains(typed));
    match f.query(AuthQuery::GetCredentialStatus {}) {
        AuthReply::Status(status) => {
            let slot = status.slots.iter().find(|s| s.profile == "cur").unwrap();
            assert!(matches!(slot.state, CredentialState::Present { .. }));
            assert_eq!(slot.login, Some(LoginMethod::Cursor));
        }
        other => panic!("{other:?}"),
    }
    // A provider that refuses the key leaves nothing stored.
    f.store.values.lock().unwrap().clear();
    f.run(save_key("cursor-2", "cur", REFUSED_KEY)).unwrap();
    assert_eq!(
        f.settled("cursor-2").await.operation.state,
        OperationState::Failed {
            code: ErrorCode::InvalidRequest
        }
    );
    assert_eq!(f.store.value("cursor-acct"), None);
}

fn delete(id: &str, profile: &str, account: &str, shared: &[&str]) -> AuthCommand {
    AuthCommand::DeleteCredential(DeleteCredential {
        operation_id: op(id),
        profile: profile.into(),
        account: account.into(),
        shared_with: shared.iter().map(|name| (*name).to_owned()).collect(),
    })
}

impl Fixture {
    /// The configuration file changed behind the core's back: nothing is refreshed.
    fn rewrite(&self, edit: impl FnOnce(&str) -> String) {
        let path = self._dir.path().join("config.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, edit(&text)).unwrap();
    }

    fn put(&self, account: &str) {
        self.store
            .values
            .lock()
            .unwrap()
            .insert(account.to_owned(), "secret".to_owned());
    }

    async fn deleted(&mut self, id: &str, command: AuthCommand) -> OperationState {
        self.run(command).unwrap();
        self.settled(id).await.operation.state
    }

    fn account_of(&self, profile: &str) -> Option<String> {
        match self.query(AuthQuery::GetCredentialStatus {}) {
            AuthReply::Status(status) => status
                .slots
                .into_iter()
                .find(|slot| slot.profile == profile)
                .and_then(|slot| match slot.storage {
                    workbench_protocol::namespaces::settings::CredentialRef::Keyring {
                        account,
                        ..
                    } => Some(account),
                    _ => None,
                }),
            other => panic!("{other:?}"),
        }
    }
}

const STALE: OperationState = OperationState::Failed {
    code: ErrorCode::StaleRevision,
};

/// A deletion goes through when the account and the providers sharing it are what the person
/// confirmed, and the file is read again for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_matching_its_confirmation_deletes() {
    let mut f = Fixture::new();
    f.put("key-acct");
    let state = f
        .deleted("d-1", delete("d-1", "key", "key-acct", &[]))
        .await;
    assert_eq!(state, OperationState::Ready);
    assert_eq!(f.store.value("key-acct"), None);
}

/// The file moved the provider to another account without a refresh: the confirmation named the
/// old one, so nothing is deleted, under either account, and the status shows what the file says.
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_confirmed_for_an_account_the_file_no_longer_names_deletes_nothing() {
    let mut f = Fixture::new();
    f.put("key-acct");
    f.put("key-acct-2");
    f.rewrite(|text| text.replace("account = 'key-acct'", "account = 'key-acct-2'"));
    let state = f
        .deleted("d-1", delete("d-1", "key", "key-acct", &[]))
        .await;
    assert_eq!(state, STALE);
    assert!(f.store.value("key-acct").is_some());
    assert!(f.store.value("key-acct-2").is_some());
    assert_eq!(
        f.account_of("key").as_deref(),
        Some("key-acct-2"),
        "the failed deletion brought the status up to the file"
    );
    // Confirmed again against the new account, it goes through.
    let state = f
        .deleted("d-2", delete("d-2", "key", "key-acct-2", &[]))
        .await;
    assert_eq!(state, OperationState::Ready);
    assert_eq!(f.store.value("key-acct-2"), None);
    assert!(f.store.value("key-acct").is_some());
}

/// Another provider was pointed at the same account in the file without a refresh: a deletion
/// that did not list it deletes nothing; one that lists it goes through. The service does not
/// matter, the account is what the credential store keys on.
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_that_does_not_list_everyone_who_loses_the_credential_deletes_nothing() {
    for service in ["evorch", "another-service"] {
        let mut f = Fixture::new();
        f.put("key-acct");
        f.rewrite(|text| {
            text.replace(
                "credential = { type = 'keyring', service = 'evorch', account = 'cursor-acct' }",
                &format!(
                    "credential = {{ type = 'keyring', service = '{service}', account = 'key-acct' }}"
                ),
            )
        });
        let state = f
            .deleted("d-1", delete("d-1", "key", "key-acct", &[]))
            .await;
        assert_eq!(state, STALE, "{service}");
        assert!(f.store.value("key-acct").is_some(), "{service}");
        let state = f
            .deleted("d-2", delete("d-2", "key", "key-acct", &["cur"]))
            .await;
        assert_eq!(state, OperationState::Ready, "{service}");
        assert_eq!(f.store.value("key-acct"), None, "{service}");
    }
}

/// A provider listed as sharing that does not is as much a difference as one missing.
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_that_lists_a_provider_that_no_longer_shares_deletes_nothing() {
    let mut f = Fixture::new();
    f.put("key-acct");
    let state = f
        .deleted("d-1", delete("d-1", "key", "key-acct", &["cur"]))
        .await;
    assert_eq!(state, STALE);
    assert!(f.store.value("key-acct").is_some());
}

/// The same id with another confirmation is another request.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_operation_id_with_another_confirmation_conflicts() {
    let mut f = Fixture::new();
    f.put("key-acct");
    f.run(delete("d-1", "key", "key-acct", &[])).unwrap();
    f.settled("d-1").await;
    let other = [
        delete("d-1", "key", "other-acct", &[]),
        delete("d-1", "key", "key-acct", &["cur"]),
    ];
    for command in other {
        assert_eq!(f.run(command).unwrap_err().code, ErrorCode::Conflict);
    }
    // The order of the providers is not part of what was confirmed.
    f.run(delete("d-2", "key", "key-acct", &["a", "b"]))
        .unwrap();
    f.run(delete("d-2", "key", "key-acct", &["b", "a"]))
        .unwrap();
}
