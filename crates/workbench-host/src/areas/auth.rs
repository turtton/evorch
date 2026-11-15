//! Host service of the `auth` area (PR 10): which credentials exist, saving and
//! deleting a key, and the browser logins of the subscription providers.
//!
//! The core is the only place a credential is written. A secret reaches this
//! service inside a command and goes straight to the credential store: it is
//! never put in an operation, an event, a log line or the digest that tells a
//! repeated request from a new one.
//!
//! A login has three sections. Preparing it (the URL, the loopback listener)
//! and exchanging the callback for credentials run under the restart permit
//! the command was accepted with, or one taken again for the exchange. The
//! section between them waits for a person and holds no permit: a planned
//! restart or shutdown ends it as `Cancelled(Restart | Shutdown)` without
//! waiting, and if the gate refuses at the exchange nothing is written.
mod login;
mod provider_logins;

pub use login::{BoxFuture, Callback, LoginFailure, LoginFlow, LoginStarter};
pub use provider_logins::ProviderLogins;

use super::{
    AreaHandles, AreaTasks, ProjectionSource, StopSignal,
    settings::operations::{Lookup, Operations, Tracked, digest_of},
    settings::store::{credential_ref, provider_kind},
};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::{
    CommandAccess, HostCommandPermit, HostRestartGate, HostRestartPhase,
};
use login::Session;
use sandbox::{CredentialStore, Secret};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    events::UiEvent,
    ids::OperationId,
    namespaces::auth::{
        AuthCommand, AuthEvent, AuthOperationKind, AuthOperationView, AuthStatus, CredentialSlot,
        CredentialState, LoginInputOutcome, LoginInputRejection, LoginMethod, LoginPhase,
        LoginView, MAX_OPERATIONS, MAX_SLOTS,
    },
    operation::{CancelReason, OperationState, OperationView},
    wire::Revision,
};

/// How long a login waits for the person.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

impl Tracked for AuthOperationView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
}

/// The `auth` service, reached from `ingress/namespaces/auth.rs` as
/// `self.areas.auth`. Without handles (the unwired form) every operation is
/// refused.
#[derive(Default, Clone)]
pub struct AuthArea {
    inner: Option<Arc<Inner>>,
}

/// What a service needs, all of it from the bootstrap's handles except the
/// starter, which production fills with [`ProviderLogins`].
pub struct AuthParts {
    pub config_dir: std::path::PathBuf,
    /// The configuration the core started with.
    pub startup: Arc<config::Config>,
    pub credentials: Arc<dyn CredentialStore>,
    pub tasks: AreaTasks,
    pub source: ProjectionSource,
    pub starter: Arc<dyn LoginStarter>,
    pub login_timeout: Duration,
}

struct Profile {
    name: String,
    kind: config::ProviderTypeConfig,
    credential: config::CredentialRefConfig,
}

struct Inner {
    config_dir: std::path::PathBuf,
    startup: Arc<config::Config>,
    credentials: Arc<dyn CredentialStore>,
    tasks: AreaTasks,
    source: ProjectionSource,
    starter: Arc<dyn LoginStarter>,
    login_timeout: Duration,
    state: Mutex<State>,
}

struct State {
    operations: Operations<AuthOperationView>,
    status: AuthStatus,
    profiles: Vec<Profile>,
    sessions: HashMap<OperationId, Arc<Session>>,
}

/// What an accepted command answers.
pub enum Submitted {
    Operation {
        view: AuthOperationView,
        /// The same operation id and request were accepted before.
        already_applied: bool,
    },
    LoginInput(LoginInputOutcome),
}

struct Failure {
    code: ErrorCode,
    message: &'static str,
}

fn error(code: ErrorCode, message: &str, retry: RetryAdvice) -> WorkbenchError {
    WorkbenchError::new(code, message, retry)
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn options(dir: &std::path::Path) -> config::LoadOptions {
    config::LoadOptions {
        user_config_dir: Some(dir.to_path_buf()),
        read_env: false,
        ..Default::default()
    }
}

fn read_profiles(dir: &std::path::Path, startup: &config::Config) -> Vec<Profile> {
    let loaded = config::Config::load_unresolved(&options(dir));
    let config = loaded.as_ref().unwrap_or(startup);
    config
        .providers
        .iter()
        .map(|(name, profile)| Profile {
            name: name.clone(),
            kind: profile.provider_type,
            credential: profile.credential.clone(),
        })
        .collect()
}

/// What the subscription login of `kind` is, if it has one.
fn login_method(kind: config::ProviderTypeConfig) -> Option<LoginMethod> {
    match kind {
        config::ProviderTypeConfig::OpenAiCodex => Some(LoginMethod::Codex),
        config::ProviderTypeConfig::AnthropicSubscription => Some(LoginMethod::Claude),
        config::ProviderTypeConfig::Cursor => Some(LoginMethod::Cursor),
        _ => None,
    }
}

fn account_of(credential: &config::CredentialRefConfig) -> Option<&str> {
    match credential {
        config::CredentialRefConfig::Keyring { account, .. } => Some(account),
        config::CredentialRefConfig::Env { .. } => None,
    }
}

fn expiry(kind: config::ProviderTypeConfig, secret: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(secret).ok()?;
    match kind {
        config::ProviderTypeConfig::OpenAiCodex => {
            let token = value.get("id_token")?.as_str()?;
            provider_logins::exp_of(token)
        }
        config::ProviderTypeConfig::AnthropicSubscription | config::ProviderTypeConfig::Cursor => {
            value.get("expires_at")?.as_u64()
        }
        _ => None,
    }
}

/// Read the state of every slot. Blocks on the credential store.
fn read_slots(profiles: &[Profile], credentials: &dyn CredentialStore) -> Vec<CredentialSlot> {
    profiles
        .iter()
        .take(MAX_SLOTS)
        .map(|profile| {
            let state = match &profile.credential {
                config::CredentialRefConfig::Keyring { account, .. } => {
                    match credentials.get(account) {
                        Ok(Some(secret)) => CredentialState::Present {
                            expires_at_unix: expiry(profile.kind, secret.expose()),
                        },
                        Ok(None) => CredentialState::Missing,
                        Err(_) => CredentialState::Unavailable,
                    }
                }
                config::CredentialRefConfig::Env { var } => {
                    if std::env::var_os(var).is_some_and(|value| !value.is_empty()) {
                        CredentialState::Present {
                            expires_at_unix: None,
                        }
                    } else {
                        CredentialState::Missing
                    }
                }
            };
            CredentialSlot {
                profile: profile.name.clone(),
                kind: provider_kind(profile.kind),
                storage: credential_ref(&profile.credential),
                state,
                login: login_method(profile.kind),
            }
        })
        .collect()
}

fn failure_code(failure: LoginFailure) -> ErrorCode {
    match failure {
        LoginFailure::PortBusy | LoginFailure::Network => ErrorCode::Busy,
        LoginFailure::Rejected => ErrorCode::InvalidRequest,
        LoginFailure::Unavailable => ErrorCode::Internal,
        LoginFailure::StoreUnavailable => ErrorCode::StorageUnavailable,
    }
}

impl AuthArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("auth")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::with_parts(AuthParts {
            config_dir: handles.config_dir.clone(),
            startup: handles.config.clone(),
            credentials: handles.credentials.clone(),
            tasks: handles.tasks.clone(),
            source,
            starter: Arc::new(ProviderLogins::new(handles.credentials.clone())),
            login_timeout: LOGIN_TIMEOUT,
        }))
    }

    /// A service over `parts`; what the bootstrap does and what a test of the
    /// area does. The credential store is not read here: a keychain can stall,
    /// so the first `refresh_credentials` fills in the states.
    pub fn with_parts(parts: AuthParts) -> Self {
        let profiles = read_profiles(&parts.config_dir, &parts.startup);
        let slots = profiles
            .iter()
            .take(MAX_SLOTS)
            .map(|profile| CredentialSlot {
                profile: profile.name.clone(),
                kind: provider_kind(profile.kind),
                storage: credential_ref(&profile.credential),
                state: CredentialState::Unknown,
                login: login_method(profile.kind),
            })
            .collect();
        Self {
            inner: Some(Arc::new(Inner {
                config_dir: parts.config_dir,
                startup: parts.startup,
                credentials: parts.credentials,
                tasks: parts.tasks,
                source: parts.source,
                starter: parts.starter,
                login_timeout: parts.login_timeout,
                state: Mutex::new(State {
                    operations: Operations::new(MAX_OPERATIONS),
                    status: AuthStatus {
                        revision: Revision(0),
                        slots,
                    },
                    profiles,
                    sessions: HashMap::new(),
                }),
            })),
        }
    }

    fn inner(&self) -> Result<&Arc<Inner>, WorkbenchError> {
        self.inner.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "credential management is not available in this host",
                RetryAdvice::Never,
            )
        })
    }

    /// Accept a command.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request or a second login of
    /// one profile, `NotFound` for an unknown profile or login,
    /// `InvalidRequest` for a profile the command does not fit,
    /// `LimitExceeded` when every retained operation is still running,
    /// `Draining` once the host is stopping.
    pub(crate) fn submit(
        &self,
        command: AuthCommand,
        accepted: Option<HostCommandPermit>,
        gate: HostRestartGate,
    ) -> Result<Submitted, WorkbenchError> {
        let inner = self.inner()?;
        match command {
            AuthCommand::SubmitLoginInput(input) => {
                let session = inner.session(&input.login_id);
                let outcome = match session {
                    Some(session) => {
                        let outcome = session.submit(input.input.expose(), Instant::now());
                        if outcome == LoginInputOutcome::Accepted {
                            inner.publish(AuthEvent::LoginChanged {
                                login_id: input.login_id,
                            });
                        }
                        outcome
                    }
                    None => LoginInputOutcome::Rejected {
                        reason: LoginInputRejection::UnknownLogin,
                    },
                };
                Ok(Submitted::LoginInput(outcome))
            }
            AuthCommand::CancelLogin(cancel) => inner.cancel(&cancel.login_id),
            AuthCommand::StartLogin(start) => {
                inner.start_login(start.operation_id, &start.profile, accepted, gate)
            }
            AuthCommand::RefreshCredentials(refresh) => inner.start_operation(
                refresh.operation_id,
                AuthOperationKind::RefreshStatus,
                None,
                digest_of(&[b"refresh"]),
                accepted,
                Work::Refresh,
            ),
            AuthCommand::SaveApiKey(save) => inner.start_operation(
                save.operation_id,
                AuthOperationKind::SaveApiKey,
                Some(save.profile.clone()),
                // The value is left out: the same id with another value is the
                // same request, answered from the table, never run twice.
                digest_of(&[b"save_api_key", save.profile.as_bytes()]),
                accepted,
                Work::Save {
                    profile: save.profile,
                    value: Secret::from(save.value.expose().to_owned()),
                },
            ),
            AuthCommand::DeleteCredential(delete) => inner.start_operation(
                delete.operation_id,
                AuthOperationKind::DeleteCredential,
                Some(delete.profile.clone()),
                digest_of(&[b"delete_credential", delete.profile.as_bytes()]),
                accepted,
                Work::Delete {
                    profile: delete.profile,
                },
            ),
        }
    }

    /// The credential status the core holds.
    ///
    /// # Errors
    /// Only the unwired form refuses.
    pub fn status(&self) -> Result<AuthStatus, WorkbenchError> {
        Ok(lock(&self.inner()?.state).status.clone())
    }

    /// One login, running or finished.
    ///
    /// # Errors
    /// `NotFound` for a login the core does not hold.
    pub fn login(&self, id: &OperationId) -> Result<LoginView, WorkbenchError> {
        self.inner()?
            .session(id)
            .map(|session| session.view())
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown login", RetryAdvice::Never))
    }

    /// One operation, running or finished.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub fn operation(&self, id: &OperationId) -> Result<AuthOperationView, WorkbenchError> {
        lock(&self.inner()?.state)
            .operations
            .get(id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown operation", RetryAdvice::Never))
    }

    /// Wait until `id` is settled and return it.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub async fn wait_operation(
        &self,
        id: &OperationId,
    ) -> Result<AuthOperationView, WorkbenchError> {
        let mut changes = lock(&self.inner()?.state).operations.subscribe();
        loop {
            let view = self.operation(id)?;
            if view.operation.state.is_final() {
                return Ok(view);
            }
            if changes.changed().await.is_err() {
                return Ok(view);
            }
        }
    }

    /// The retained operations, newest first.
    ///
    /// # Errors
    /// Only the unwired form refuses.
    pub fn operations(&self) -> Result<Vec<AuthOperationView>, WorkbenchError> {
        Ok(lock(&self.inner()?.state).operations.list())
    }
}

enum Work {
    Refresh,
    Save { profile: String, value: Secret },
    Delete { profile: String },
}

enum Settle {
    Ready,
    Failed(LoginFailure),
    TimedOut,
    Cancelled(CancelReason),
}

enum Waited {
    Callback,
    Ended(Option<Settle>),
}

fn restart_reason(gate: &HostRestartGate) -> Option<CancelReason> {
    match gate.phase() {
        HostRestartPhase::Open => None,
        HostRestartPhase::ShuttingDown => Some(CancelReason::Shutdown),
        _ => Some(CancelReason::Restart),
    }
}

impl Inner {
    fn publish(&self, event: AuthEvent) {
        if let Err(error) = self.source.publish(UiEvent::Auth(event)) {
            tracing::warn!(code = ?error.code, "auth event could not be published");
        }
    }

    fn session(&self, id: &OperationId) -> Option<Arc<Session>> {
        lock(&self.state).sessions.get(id).cloned()
    }

    fn operation_view(&self, id: &OperationId) -> Option<AuthOperationView> {
        lock(&self.state).operations.get(id).cloned()
    }

    /// Register a new operation, or answer from the table.
    fn register(
        &self,
        id: &OperationId,
        kind: AuthOperationKind,
        profile: Option<String>,
        digest: [u8; 32],
    ) -> Result<Result<AuthOperationView, Submitted>, WorkbenchError> {
        let mut state = lock(&self.state);
        match state.operations.lookup(id, &digest) {
            Lookup::Same(view) => {
                return Ok(Err(Submitted::Operation {
                    view,
                    already_applied: true,
                }));
            }
            Lookup::Conflict => {
                return Err(error(
                    ErrorCode::Conflict,
                    "this operation id was used for a different request",
                    RetryAdvice::Never,
                ));
            }
            Lookup::New => {}
        }
        let revision = state.operations.next_revision();
        let view = AuthOperationView {
            operation: OperationView {
                operation_id: id.clone(),
                state: OperationState::Pending,
                revision,
                result: None,
            },
            kind,
            profile,
            message: None,
        };
        state.operations.insert(view.clone(), digest)?;
        Ok(Ok(view))
    }

    fn start_operation(
        self: &Arc<Self>,
        id: OperationId,
        kind: AuthOperationKind,
        profile: Option<String>,
        digest: [u8; 32],
        accepted: Option<HostCommandPermit>,
        work: Work,
    ) -> Result<Submitted, WorkbenchError> {
        let view = match self.register(&id, kind, profile, digest)? {
            Ok(view) => view,
            Err(answer) => return Ok(answer),
        };
        let task = self.clone();
        let task_id = id.clone();
        let spawned = self
            .tasks
            .spawn_counted_tracked("auth-operation", accepted, move |_stop| {
                task.run_operation(task_id, work)
            });
        if let Err(refused) = spawned {
            lock(&self.state).operations.forget(&id);
            return Err(error(
                ErrorCode::Draining,
                &refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Submitted::Operation {
            view,
            already_applied: false,
        })
    }

    async fn blocking<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Failure> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| Failure {
                code: ErrorCode::Internal,
                message: "the credential task did not finish",
            })
    }

    async fn run_operation(
        self: Arc<Self>,
        id: OperationId,
        work: Work,
    ) -> Result<(), BootstrapError> {
        let outcome = self.execute(work).await;
        self.finish(&id, outcome);
        Ok(())
    }

    fn profile(&self, name: &str) -> Option<Profile> {
        let find = |profiles: &[Profile]| {
            profiles.iter().find(|p| p.name == name).map(|p| Profile {
                name: p.name.clone(),
                kind: p.kind,
                credential: p.credential.clone(),
            })
        };
        let known = find(&lock(&self.state).profiles);
        known.or_else(|| {
            let fresh = read_profiles(&self.config_dir, &self.startup);
            let found = find(&fresh);
            lock(&self.state).profiles = fresh;
            found
        })
    }

    async fn execute(&self, work: Work) -> Result<(), Failure> {
        match work {
            Work::Refresh => {}
            Work::Save { profile, value } => {
                let (account, kind) = self.key_account(&profile)?;
                if login_method(kind).is_some() {
                    return Err(Failure {
                        code: ErrorCode::InvalidRequest,
                        message: "this provider signs in with a login, not an API key",
                    });
                }
                let store = self.credentials.clone();
                Self::blocking(move || store.set(&account, &value))
                    .await?
                    .map_err(|_| Failure {
                        code: ErrorCode::StorageUnavailable,
                        message: "the credential could not be saved",
                    })?;
            }
            Work::Delete { profile } => {
                let (account, _) = self.key_account(&profile)?;
                let store = self.credentials.clone();
                Self::blocking(move || store.delete(&account))
                    .await?
                    .map_err(|_| Failure {
                        code: ErrorCode::StorageUnavailable,
                        message: "the credential could not be deleted",
                    })?;
            }
        }
        self.reread().await
    }

    /// The keychain account a profile's key lives under.
    fn key_account(&self, name: &str) -> Result<(String, config::ProviderTypeConfig), Failure> {
        let profile = self.profile(name).ok_or(Failure {
            code: ErrorCode::NotFound,
            message: "unknown profile",
        })?;
        let account = account_of(&profile.credential).ok_or(Failure {
            code: ErrorCode::InvalidRequest,
            message: "this profile reads its key from an environment variable",
        })?;
        Ok((account.to_owned(), profile.kind))
    }

    /// Read the profiles and the credential store again into the status, and
    /// announce a change.
    async fn reread(&self) -> Result<(), Failure> {
        let (dir, startup, credentials) = (
            self.config_dir.clone(),
            self.startup.clone(),
            self.credentials.clone(),
        );
        let (profiles, slots) = Self::blocking(move || {
            let profiles = read_profiles(&dir, &startup);
            let slots = read_slots(&profiles, credentials.as_ref());
            (profiles, slots)
        })
        .await?;
        let changed = {
            let mut state = lock(&self.state);
            state.profiles = profiles;
            let changed = state.status.slots != slots;
            if changed {
                state.status.revision = Revision(state.status.revision.0 + 1);
                state.status.slots = slots;
            }
            changed.then(|| state.status.revision)
        };
        if let Some(revision) = changed {
            self.publish(AuthEvent::CredentialStatusChanged { revision });
        }
        Ok(())
    }

    fn finish(&self, id: &OperationId, outcome: Result<(), Failure>) {
        let view = lock(&self.state)
            .operations
            .update(id, |view| match outcome {
                Ok(()) => view.operation.state = OperationState::Ready,
                Err(failure) => {
                    view.operation.state = OperationState::Failed { code: failure.code };
                    view.message = Some(failure.message.to_owned());
                }
            });
        if let Some(view) = view {
            self.publish(AuthEvent::AuthOperationCompleted { operation: view });
        }
    }

    // ---- logins -----------------------------------------------------------

    fn start_login(
        self: &Arc<Self>,
        id: OperationId,
        profile_name: &str,
        accepted: Option<HostCommandPermit>,
        gate: HostRestartGate,
    ) -> Result<Submitted, WorkbenchError> {
        let digest = digest_of(&[b"start_login", profile_name.as_bytes()]);
        if let Some(view) = self.operation_view(&id) {
            return match digest_matches(&view, profile_name) {
                true => Ok(Submitted::Operation {
                    view,
                    already_applied: true,
                }),
                false => Err(error(
                    ErrorCode::Conflict,
                    "this operation id was used for a different request",
                    RetryAdvice::Never,
                )),
            };
        }
        let profile = self
            .profile(profile_name)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown profile", RetryAdvice::Never))?;
        let method = login_method(profile.kind).ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "this provider has no login",
                RetryAdvice::Never,
            )
        })?;
        let account = account_of(&profile.credential).ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "this profile has no keychain account for a login",
                RetryAdvice::Never,
            )
        })?;
        if lock(&self.state).sessions.values().any(|session| {
            session.profile == profile_name && session.phase() != LoginPhase::Settled
        }) {
            return Err(error(
                ErrorCode::Conflict,
                "a login for this profile is already running",
                RetryAdvice::UserActionRequired,
            ));
        }
        let flow = self.starter.begin(method, account).map_err(|failure| {
            error(
                failure_code(failure),
                failure.message(),
                RetryAdvice::UserActionRequired,
            )
        })?;
        let view = match self.register(
            &id,
            AuthOperationKind::Login,
            Some(profile_name.to_owned()),
            digest,
        )? {
            Ok(view) => view,
            Err(answer) => return Ok(answer),
        };
        let session = Arc::new(Session::new(
            id.clone(),
            profile_name.to_owned(),
            method,
            flow,
            Instant::now() + self.login_timeout,
        ));
        {
            let mut state = lock(&self.state);
            let State {
                operations,
                sessions,
                ..
            } = &mut *state;
            sessions.retain(|known, _| operations.get(known).is_some());
            sessions.insert(id.clone(), session.clone());
        }
        let task = self.clone();
        let task_session = session.clone();
        let spawned = self.tasks.spawn("auth-login", move |stop| {
            task.run_login(task_session, gate, accepted, stop)
        });
        if let Err(refused) = spawned {
            session.claim();
            let mut state = lock(&self.state);
            state.operations.forget(&id);
            state.sessions.remove(&id);
            return Err(error(
                ErrorCode::Draining,
                &refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Submitted::Operation {
            view,
            already_applied: false,
        })
    }

    fn cancel(&self, login_id: &OperationId) -> Result<Submitted, WorkbenchError> {
        let session = self
            .session(login_id)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown login", RetryAdvice::Never))?;
        if session.claim_while_waiting() {
            self.settle(&session, Settle::Cancelled(CancelReason::Requested));
        }
        let view = self
            .operation_view(login_id)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown login", RetryAdvice::Never))?;
        Ok(Submitted::Operation {
            view,
            already_applied: false,
        })
    }

    async fn run_login(
        self: Arc<Self>,
        session: Arc<Session>,
        gate: HostRestartGate,
        accepted: Option<HostCommandPermit>,
        mut stop: StopSignal,
    ) -> Result<(), BootstrapError> {
        // The first section is over: the session exists and the URL is
        // offered. What follows waits for a person and holds no permit.
        drop(accepted);
        let waited = self.wait(&session, &gate, &mut stop).await;
        session
            .stop_listener
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match waited {
            Waited::Ended(settle) => {
                if let Some(settle) = settle {
                    self.settle(&session, settle);
                }
            }
            Waited::Callback => self.exchange(&session, &gate).await,
        }
        Ok(())
    }

    async fn wait(
        &self,
        session: &Arc<Session>,
        gate: &HostRestartGate,
        stop: &mut StopSignal,
    ) -> Waited {
        let mut changes = gate.subscribe();
        if let Some(reason) = restart_reason(gate) {
            return self.end_waiting(session, Settle::Cancelled(reason));
        }
        let timeout = tokio::time::sleep(self.login_timeout);
        tokio::pin!(timeout);
        let loopback = session.flow.wait_loopback(session.stop_listener.clone());
        tokio::pin!(loopback);
        loop {
            tokio::select! {
                () = stop.stopped() => {
                    return self.end_waiting(session, Settle::Cancelled(CancelReason::Shutdown));
                }
                changed = changes.changed() => {
                    let reason = if changed.is_err() {
                        Some(CancelReason::Shutdown)
                    } else {
                        restart_reason(gate)
                    };
                    if let Some(reason) = reason {
                        return self.end_waiting(session, Settle::Cancelled(reason));
                    }
                }
                () = session.wake.notified() => match session.phase() {
                    LoginPhase::Waiting => {}
                    LoginPhase::Completing => return Waited::Callback,
                    LoginPhase::Settled => return Waited::Ended(None),
                },
                () = &mut timeout => return self.end_waiting(session, Settle::TimedOut),
                result = &mut loopback => {
                    return match result {
                        Ok(callback) => {
                            // `false`: a pasted input won; its callback is the one.
                            let _ = session.commit_loopback(callback);
                            Waited::Callback
                        }
                        Err(failure) => self.end_waiting(session, Settle::Failed(failure)),
                    };
                }
            }
        }
    }

    /// Ends the wait with `settle` unless a callback was committed meanwhile
    /// (then the exchange decides) or the login was settled already.
    fn end_waiting(&self, session: &Session, settle: Settle) -> Waited {
        if session.claim_while_waiting() {
            Waited::Ended(Some(settle))
        } else if session.phase() == LoginPhase::Settled {
            Waited::Ended(None)
        } else {
            Waited::Callback
        }
    }

    /// The last section: take the gate again, exchange the callback, store the
    /// credentials, settle. A gate that refuses writes nothing.
    async fn exchange(&self, session: &Arc<Session>, gate: &HostRestartGate) {
        let Some(callback) = session.take_callback() else {
            if session.claim() {
                self.settle(session, Settle::Failed(LoginFailure::Unavailable));
            }
            return;
        };
        self.publish(AuthEvent::LoginChanged {
            login_id: session.login_id.clone(),
        });
        let permit = match gate.enter(CommandAccess::NewWork) {
            Ok(permit) => permit,
            Err(_) => {
                if session.claim() {
                    let reason = restart_reason(gate).unwrap_or(CancelReason::Restart);
                    self.settle(session, Settle::Cancelled(reason));
                }
                return;
            }
        };
        let result = session.flow.complete(callback).await;
        if session.claim() {
            self.settle(
                session,
                match result {
                    Ok(_) => Settle::Ready,
                    Err(failure) => Settle::Failed(failure),
                },
            );
        }
        if result.is_ok() {
            let _ = self.reread().await;
        }
        drop(permit);
    }

    /// Write the outcome of a login this caller claimed.
    fn settle(&self, session: &Session, settle: Settle) {
        let view = lock(&self.state)
            .operations
            .update(&session.login_id, |view| match settle {
                Settle::Ready => view.operation.state = OperationState::Ready,
                Settle::Failed(failure) => {
                    view.operation.state = OperationState::Failed {
                        code: failure_code(failure),
                    };
                    view.message = Some(failure.message().to_owned());
                }
                Settle::TimedOut => {
                    view.operation.state = OperationState::Failed {
                        code: ErrorCode::Busy,
                    };
                    view.message = Some("the login timed out; start it again".to_owned());
                }
                Settle::Cancelled(reason) => {
                    view.operation.state = OperationState::Cancelled { reason };
                }
            });
        self.publish(AuthEvent::LoginChanged {
            login_id: session.login_id.clone(),
        });
        if let Some(view) = view {
            self.publish(AuthEvent::AuthOperationCompleted { operation: view });
        }
    }
}

fn digest_matches(view: &AuthOperationView, profile: &str) -> bool {
    view.kind == AuthOperationKind::Login && view.profile.as_deref() == Some(profile)
}
