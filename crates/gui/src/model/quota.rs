use providers::provider::codex::quota::{CodexQuotaClient, QuotaConfig, QuotaError, QuotaSnapshot};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[async_trait::async_trait]
pub trait QuotaBackend: Send {
    async fn fetch(&mut self) -> Result<QuotaSnapshot, QuotaError>;
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

type Update = (Box<dyn QuotaBackend>, Result<QuotaSnapshot, QuotaError>);

struct Job {
    thread: std::thread::JoinHandle<Option<Update>>,
    cancel: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
pub struct QuotaState {
    pub snapshot: Option<QuotaSnapshot>,
    pub error: Option<QuotaError>,
    backend: Option<Box<dyn QuotaBackend>>,
    job: Option<Job>,
    next_refresh: Option<Instant>,
    account: Option<String>,
    injected: bool,
}

impl std::fmt::Debug for QuotaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaState")
            .field("snapshot", &self.snapshot)
            .field("busy", &self.job.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for QuotaState {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            let _ = job.cancel.send(());
            let _ = job.thread.join();
        }
    }
}

impl QuotaState {
    pub fn with_backend(backend: Box<dyn QuotaBackend>) -> Self {
        Self {
            backend: Some(backend),
            injected: true,
            snapshot: None,
            error: None,
            job: None,
            next_refresh: None,
            account: None,
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
        *self = Self::default();
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

    pub fn accept(&mut self, result: Result<QuotaSnapshot, QuotaError>) {
        match result {
            Ok(snapshot) => {
                self.error = snapshot.last_error.clone();
                self.snapshot = Some(snapshot);
            }
            Err(error) => {
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.stale = true;
                    snapshot.last_error = Some(error.clone());
                }
                self.error = Some(error);
            }
        }
    }

    pub fn poll(&mut self, now: Instant) {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.thread.is_finished())
            && let Some(job) = self.job.take()
        {
            match job.thread.join() {
                Ok(Some((backend, result))) => {
                    self.next_refresh = Some(
                        now + backend
                            .interval()
                            .clamp(Duration::from_secs(30), Duration::from_secs(600)),
                    );
                    self.backend = Some(backend);
                    self.accept(result);
                }
                Ok(None) | Err(_) => self.accept(Err(QuotaError::Protocol("quota worker stopped"))),
            }
        }
        if self.job.is_some() || self.next_refresh.is_some_and(|next| now < next) {
            return;
        }
        let Some(mut backend) = self.backend.take() else {
            return;
        };
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        // The state owns and cancels/joins this bounded worker; frames never await I/O.
        let thread = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return Some((backend, Err(QuotaError::Process(error.kind())))),
            };
            runtime.block_on(async {
                tokio::select! {
                    _ = cancelled => None,
                    result = backend.fetch() => Some((backend, result)),
                }
            })
        });
        self.job = Some(Job { thread, cancel });
    }
}
