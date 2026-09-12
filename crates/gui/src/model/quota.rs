use providers::provider::codex::quota::{CodexQuotaClient, QuotaConfig, QuotaError, QuotaSnapshot};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
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

type Update = (Result<QuotaSnapshot, QuotaError>, Duration);

struct Job {
    thread: std::thread::JoinHandle<()>,
    requests: mpsc::SyncSender<()>,
    updates: mpsc::Receiver<Update>,
    stopping: Arc<AtomicBool>,
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
    in_flight: bool,
}

impl std::fmt::Debug for QuotaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaState")
            .field("snapshot", &self.snapshot)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl Drop for QuotaState {
    fn drop(&mut self) {
        self.stop();
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
            in_flight: false,
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
        self.backend = None;
        if let Some(job) = &self.job {
            job.stopping.store(true, Ordering::Release);
            let _ = job.requests.try_send(());
        }
    }
}

fn start_worker(mut backend: Box<dyn QuotaBackend>) -> Job {
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
