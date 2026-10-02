use crate::model::provider_settings::{ProviderKind, ProviderSettingsModel};
use providers::provider::codex::quota::{CodexQuotaClient, QuotaConfig, QuotaError, QuotaSnapshot};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[async_trait::async_trait]
pub trait QuotaBackend<T = QuotaSnapshot>: Send {
    async fn fetch(&mut self) -> Result<T, QuotaError>;
    fn interval(&self) -> Duration;
}

#[async_trait::async_trait]
impl QuotaBackend for CodexQuotaClient {
    async fn fetch(&mut self) -> Result<QuotaSnapshot, QuotaError> {
        self.fetch_quota().await
    }
    fn interval(&self) -> Duration {
        self.poll_interval()
    }
}

pub trait QuotaData: std::fmt::Debug + Send + 'static {
    fn last_error(&self) -> Option<QuotaError>;
    fn mark_stale(&mut self, error: QuotaError);
}

impl QuotaData for QuotaSnapshot {
    fn last_error(&self) -> Option<QuotaError> {
        self.last_error.clone()
    }
    fn mark_stale(&mut self, error: QuotaError) {
        self.stale = true;
        self.last_error = Some(error);
    }
}

type Update<T> = (Result<T, QuotaError>, Duration);

struct Job<T> {
    thread: std::thread::JoinHandle<()>,
    requests: mpsc::SyncSender<()>,
    updates: mpsc::Receiver<Update<T>>,
    stopping: Arc<AtomicBool>,
}

pub struct QuotaState<T: QuotaData = QuotaSnapshot> {
    /// Per-profile subscriptions. Empty for a standalone/injected quota worker.
    pub subscriptions: BTreeMap<String, Self>,
    pub snapshot: Option<T>,
    pub error: Option<QuotaError>,
    backend: Option<Box<dyn QuotaBackend<T>>>,
    job: Option<Job<T>>,
    next_refresh: Option<Instant>,
    account: Option<String>,
    injected: bool,
    in_flight: bool,
}

impl<T: QuotaData> std::fmt::Debug for QuotaState<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaState")
            .field("snapshot", &self.snapshot)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl<T: QuotaData> Drop for QuotaState<T> {
    fn drop(&mut self) {
        self.stop();
    }
}

impl<T: QuotaData> QuotaState<T> {
    pub fn with_backend(backend: Box<dyn QuotaBackend<T>>) -> Self {
        Self {
            subscriptions: BTreeMap::new(),
            backend: Some(backend),
            injected: true,
            snapshot: None,
            error: None,
            job: None,
            next_refresh: None,
            account: None,
            in_flight: false,
        }
    }

    pub fn accept(&mut self, result: Result<T, QuotaError>) {
        match result {
            Ok(snapshot) => {
                self.error = snapshot.last_error();
                self.snapshot = Some(snapshot);
            }
            Err(error) => {
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.mark_stale(error.clone());
                }
                self.error = Some(error);
            }
        }
    }

    pub fn poll(&mut self, now: Instant) {
        for state in self.subscriptions.values_mut() {
            state.poll(now);
        }
        if let Some(job) = &self.job {
            if job.stopping.load(Ordering::Acquire) {
                if !job.thread.is_finished() {
                    return;
                }
                if let Some(job) = self.job.take() {
                    let _ = job.thread.join();
                }
                self.in_flight = false;
                self.next_refresh = None;
            } else {
                match job.updates.try_recv() {
                    Ok((result, interval)) => {
                        self.in_flight = false;
                        self.next_refresh = Some(now + interval);
                        self.accept(result);
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.in_flight = false;
                        self.accept(Err(QuotaError::Protocol("quota worker stopped")));
                        self.stop();
                        return;
                    }
                }
            }
        }
        if self.job.is_none()
            && let Some(backend) = self.backend.take()
        {
            self.job = Some(start_worker(backend));
        }
        if self.in_flight || self.next_refresh.is_some_and(|next| now < next) {
            return;
        }
        if let Some(job) = &self.job
            && job.requests.try_send(()).is_ok()
        {
            self.in_flight = true;
        }
    }

    pub const fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub fn stop(&mut self) {
        for state in self.subscriptions.values_mut() {
            state.stop();
        }
        self.backend = None;
        if let Some(job) = &self.job {
            job.stopping.store(true, Ordering::Release);
            let _ = job.requests.try_send(());
        }
    }
}

fn start_worker<T: QuotaData>(mut backend: Box<dyn QuotaBackend<T>>) -> Job<T> {
    let (requests, rx) = mpsc::sync_channel(1);
    let (tx, updates) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stopping);
    let thread = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = tx.send((
                    Err(QuotaError::Process(error.kind())),
                    Duration::from_secs(60),
                ));
                return;
            }
        };
        while rx.recv().is_ok() {
            if stopped.load(Ordering::Acquire) {
                break;
            }
            // Never cancel the fetch future: both sources can take ~20s and need child cleanup.
            let result = runtime.block_on(backend.fetch());
            let interval = backend.interval();
            if stopped.load(Ordering::Acquire) || tx.send((result, interval)).is_err() {
                break;
            }
        }
        drop(backend);
    });
    Job {
        thread,
        requests,
        updates,
        stopping,
    }
}

impl<T: QuotaData> Default for QuotaState<T> {
    fn default() -> Self {
        Self {
            subscriptions: BTreeMap::new(),
            snapshot: None,
            error: None,
            backend: None,
            job: None,
            next_refresh: None,
            account: None,
            injected: false,
            in_flight: false,
        }
    }
}

impl QuotaState {
    pub fn configure_profiles(
        &mut self,
        settings: &ProviderSettingsModel,
        store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) {
        if self.injected {
            return;
        }
        self.subscriptions.retain(|name, _| {
            settings.profiles.iter().any(|profile| {
                profile.name == *name && profile.kind == ProviderKind::CodexSubscription
            })
        });
        for profile in &settings.profiles {
            if profile.kind != ProviderKind::CodexSubscription {
                continue;
            }
            let account = match settings.credential(&profile.name) {
                Some(config::CredentialRefConfig::Keyring { account, .. }) => {
                    Some(account.as_str())
                }
                _ => None,
            };
            let state = self.subscriptions.entry(profile.name.clone()).or_default();
            state.configure(account, store.clone());
        }
    }

    pub fn configure(
        &mut self,
        account: Option<&str>,
        store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) {
        if self.injected || self.account.as_deref() == account {
            return;
        }
        self.stop();
        self.snapshot = None;
        self.error = None;
        self.account = None;
        let (Some(account), Some(store)) = (account, store) else {
            return;
        };
        self.account = Some(account.into());
        let store = Arc::new(routing::factory::CredentialStoreTokenStore::new(
            store,
            account.into(),
        ));
        match CodexQuotaClient::new(QuotaConfig::default(), store) {
            Ok(client) => self.backend = Some(Box::new(client)),
            Err(error) => self.error = Some(error),
        }
    }
}
