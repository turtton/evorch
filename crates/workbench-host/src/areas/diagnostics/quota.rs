//! One lazy, non-counted supervisor per core. Fetch futures are polled here,
//! not spawned per connection or profile. Nothing holds a host/settings lock
//! across a fetch. Generation checks also protect against late old-account data.
use super::super::AreaTasks;
use crate::bootstrap::restart::{HostRestartGate, HostRestartPhase};
use config::{CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig};
use futures_util::{
    FutureExt, StreamExt,
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};
use workbench_protocol::namespaces::diagnostics::*;

mod adapter;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Profile {
    pub name: String,
    pub kind: QuotaProvider,
    pub base_url: String,
    pub credential: CredentialRefConfig,
}

/// #852 is not in the stacked base yet. Replace only this predicate with
/// profile.show_quota_enabled() when it lands; never invent a second setting.
fn quota_enabled(profile: &ProviderProfileConfig) -> bool {
    matches!(
        profile.provider_type,
        ProviderTypeConfig::OpenAiCodex
            | ProviderTypeConfig::AnthropicSubscription
            | ProviderTypeConfig::Cursor
            | ProviderTypeConfig::KimiSubscription
    )
}

pub(super) fn profiles(config: &config::Config) -> Vec<Profile> {
    config
        .providers
        .iter()
        .filter(|(_, p)| quota_enabled(p))
        .filter_map(|(name, p)| {
            let kind = match p.provider_type {
                ProviderTypeConfig::OpenAiCodex => QuotaProvider::Codex,
                ProviderTypeConfig::AnthropicSubscription => QuotaProvider::Claude,
                ProviderTypeConfig::Cursor => QuotaProvider::Cursor,
                ProviderTypeConfig::KimiSubscription => QuotaProvider::Kimi,
                _ => return None,
            };
            if name.is_empty()
                || name.len() > workbench_protocol::limits::MAX_NAME_BYTES
                || name.chars().any(char::is_control)
            {
                return None;
            }
            Some(Profile {
                name: name.clone(),
                kind,
                base_url: p.base_url.clone(),
                credential: p.credential.clone(),
            })
        })
        .take(MAX_PROVIDER_QUOTAS)
        .collect()
}

#[async_trait::async_trait]
pub(super) trait Backend: Send {
    async fn fetch(&mut self) -> Result<Vec<QuotaWindow>, QuotaIssue>;
    /// Codex delegates to its native poll_interval (including backoff).
    fn interval(&self, failures: u32) -> Duration {
        Duration::from_secs(60)
            .saturating_mul(2_u32.saturating_pow(failures.min(4)))
            .min(Duration::from_secs(600))
    }
}
pub(super) type Fetch = Arc<dyn Fn(&Profile) -> Result<Box<dyn Backend>, QuotaIssue> + Send + Sync>;
pub(super) type Load = Arc<dyn Fn() -> Option<Vec<Profile>> + Send + Sync>;

#[derive(Default)]
pub(super) struct Quotas {
    snapshot: Arc<Mutex<ProviderQuotas>>,
    start: Arc<Notify>,
}
struct Entry {
    profile: Profile,
    generation: u64,
    backend: Option<Box<dyn Backend>>,
    cancel: Option<AbortHandle>,
    due: Instant,
    failures: u32,
    view: ProviderQuota,
}
type Finished = (
    String,
    u64,
    Box<dyn Backend>,
    Result<Vec<QuotaWindow>, QuotaIssue>,
);

pub(super) struct Parts {
    pub tasks: AreaTasks,
    pub announce: Arc<dyn Fn() + Send + Sync>,
    pub initial: Vec<Profile>,
    pub changed: watch::Receiver<u64>,
    pub load: Load,
    pub fetch: Fetch,
    pub gate: HostRestartGate,
}

impl Quotas {
    pub(super) fn new(parts: Parts) -> Result<Self, super::super::SpawnError> {
        let Parts {
            tasks,
            announce,
            initial,
            changed,
            load,
            fetch,
            gate,
        } = parts;
        let service = Self::default();
        let mut supervisor = Supervisor {
            snapshot: service.snapshot.clone(),
            announce,
            fetch,
            entries: BTreeMap::new(),
            pending: FuturesUnordered::new(),
            generation: 0,
        };
        supervisor.configure(initial);
        let start = service.start.clone();
        tasks.spawn("provider-quota-supervisor", move |mut stop| async move {
            let mut changed = changed;
            let mut restart = gate.subscribe();
            let mut active = false;
            loop {
                if gate.phase() != HostRestartPhase::Open {
                    supervisor.pending.clear();
                    for entry in supervisor.entries.values_mut() {
                        if let Some(cancel) = entry.cancel.take() { cancel.abort(); }
                        entry.due = Instant::now();
                    }
                    tokio::select! {
                        _ = stop.stopped() => break,
                        _ = restart.changed() => continue,
                    }
                }
                if active { supervisor.schedule(); }
                let due = supervisor.entries.values().filter(|e| e.cancel.is_none()).map(|e| e.due).min();
                tokio::select! {
                    biased;
                    _ = stop.stopped() => break,
                    _ = restart.changed() => {},
                    result = changed.changed() => {
                        if result.is_err() { break; }
                        let load = load.clone();
                        // ConfigStore owns composition. File I/O is not under the host mutex.
                        let loaded = tokio::task::spawn_blocking(move || load()).await;
                        if let Ok(Some(profiles)) = loaded { supervisor.configure(profiles); }
                    },
                    _ = start.notified(), if !active => { active = true; },
                    Some(result) = supervisor.pending.next(), if !supervisor.pending.is_empty() => {
                        if let Ok(done) = result { supervisor.accept(done); }
                    },
                    _ = async { if let Some(due) = due { tokio::time::sleep_until(due).await } else { std::future::pending::<()>().await } }, if active => {},
                }
            }
            // Dropping the futures cancels network I/O. No detached workers survive.
            Ok(())
        })?;
        Ok(service)
    }
    pub(super) fn get(&self) -> ProviderQuotas {
        self.start.notify_one();
        self.snapshot()
    }
    pub(super) fn snapshot(&self) -> ProviderQuotas {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
struct Supervisor {
    snapshot: Arc<Mutex<ProviderQuotas>>,
    announce: Arc<dyn Fn() + Send + Sync>,
    fetch: Fetch,
    entries: BTreeMap<String, Entry>,
    pending: FuturesUnordered<BoxFuture<'static, Result<Finished, futures_util::future::Aborted>>>,
    generation: u64,
}
impl Supervisor {
    fn publish(&self) {
        let profiles = self.entries.values().map(|e| e.view.clone()).collect();
        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ProviderQuotas { profiles };
        (self.announce)();
    }
    fn configure(&mut self, profiles: Vec<Profile>) {
        let next: BTreeMap<_, _> = profiles
            .into_iter()
            .take(MAX_PROVIDER_QUOTAS)
            .map(|p| (p.name.clone(), p))
            .collect();
        self.entries.retain(|name, entry| {
            let keep = next.get(name) == Some(&entry.profile);
            if !keep && let Some(cancel) = &entry.cancel {
                cancel.abort();
            }
            keep
        });
        for (name, profile) in next {
            if self.entries.contains_key(&name) {
                continue;
            }
            self.generation = self.generation.wrapping_add(1);
            self.entries.insert(
                name.clone(),
                Entry {
                    view: ProviderQuota {
                        profile: name,
                        provider: profile.kind,
                        state: QuotaState::Loading,
                        fetched_at_ms: None,
                        windows: vec![],
                        issue: None,
                    },
                    profile,
                    generation: self.generation,
                    backend: None,
                    cancel: None,
                    due: Instant::now(),
                    failures: 0,
                },
            );
        }
        self.publish();
    }
    fn schedule(&mut self) {
        let mut changed = false;
        for (name, entry) in &mut self.entries {
            if entry.cancel.is_some() || entry.due > Instant::now() {
                continue;
            }
            let backend = entry
                .backend
                .take()
                .map(Ok)
                .unwrap_or_else(|| (self.fetch)(&entry.profile));
            let mut backend = match backend {
                Ok(backend) => backend,
                Err(issue) => {
                    failure(entry, issue);
                    entry.due = Instant::now()
                        + Duration::from_secs(60)
                            .saturating_mul(2_u32.saturating_pow(entry.failures.min(4)))
                            .min(Duration::from_secs(600));
                    changed = true;
                    continue;
                }
            };
            let name = name.clone();
            let generation = entry.generation;
            let (cancel, registration) = AbortHandle::new_pair();
            entry.cancel = Some(cancel);
            self.pending.push(
                Abortable::new(
                    async move {
                        let result = backend.fetch().await;
                        (name, generation, backend, result)
                    },
                    registration,
                )
                .boxed(),
            );
        }
        if changed {
            self.publish();
        }
    }
    fn accept(&mut self, (name, generation, backend, result): Finished) {
        let Some(entry) = self
            .entries
            .get_mut(&name)
            .filter(|e| e.generation == generation)
        else {
            return;
        };
        entry.cancel = None;
        match result {
            Ok(windows) => {
                entry.view.windows = windows.into_iter().take(MAX_QUOTA_WINDOWS).collect();
                entry.view.fetched_at_ms = Some(super::now_ns().max(0) as u64 / 1_000_000);
                entry.view.issue = None;
                entry.view.state = QuotaState::Ready;
                entry.failures = 0;
            }
            Err(issue) => failure(entry, issue),
        }
        entry.due = Instant::now() + backend.interval(entry.failures);
        entry.backend = Some(backend);
        self.publish();
    }
}
fn failure(entry: &mut Entry, issue: QuotaIssue) {
    entry.failures = entry.failures.saturating_add(1);
    entry.view.issue = Some(issue);
    entry.view.state = if entry.view.fetched_at_ms.is_some() {
        QuotaState::Stale
    } else {
        QuotaState::Unavailable
    };
}

pub(super) fn production(store: Arc<dyn sandbox::CredentialStore>) -> Fetch {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(fetch) = test_support::injected() {
        return fetch;
    }
    Arc::new(move |profile| adapter::create(profile, store.clone()))
}
