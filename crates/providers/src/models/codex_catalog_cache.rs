//! Flights hold state/notifications, never HTTP futures or runtime-owned tasks.
use super::{CODEX_CLIENT_VERSION, CodexModelInfo, fetch_codex_models};
use crate::{ProviderAuth, ProviderError};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const TTL: Duration = Duration::from_secs(5 * 60);
const CAPACITY: usize = 128;
type Models = Result<Vec<CodexModelInfo>, ProviderError>;

// No Debug: credentials and catalog identities never belong in diagnostics.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    url: String,
    account: String,
    token_hash: [u8; 32],
    version: &'static str,
}
impl Key {
    fn new(base: &str, auth: &ProviderAuth, account: &str) -> Self {
        let url = format!("{}/models", base.trim_end_matches('/'));
        // Match the URL parser used by HTTP (e.g. default ports and dot segments),
        // without applying the Codex client's endpoint resolution to direct callers.
        let url = reqwest::Url::parse(&url).map_or(url, |url| url.to_string());
        Self {
            url,
            account: account.into(),
            token_hash: Sha256::digest(auth.api_key.as_bytes()).into(),
            version: CODEX_CLIENT_VERSION,
        }
    }
}
struct Cached {
    models: Vec<CodexModelInfo>,
    completed: Duration,
    order: u64,
}
#[derive(Default)]
struct Registry {
    completed: HashMap<Key, Cached>,
    flights: HashMap<Key, Arc<Flight>>,
    order: u64,
}
#[derive(Default)]
struct Flight {
    state: Mutex<FlightState>,
    changed: Notify,
}
#[derive(Default)]
enum FlightState {
    #[default]
    Available,
    Owned,
    Complete(Models),
    Abandoned,
}

/// Independent, non-persistent Codex catalog cache. Nonempty successes live five
/// minutes from completion (not last access), with at most 128 completed entries.
/// Errors/empty lists are shared only with callers already in the same flight.
pub struct CodexCatalogCache {
    registry: Mutex<Registry>,
    now: Arc<dyn Fn() -> Duration + Send + Sync>,
}
impl std::fmt::Debug for CodexCatalogCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCatalogCache").finish_non_exhaustive()
    }
}
impl Default for CodexCatalogCache {
    fn default() -> Self {
        Self::new()
    }
}
impl CodexCatalogCache {
    /// Creates an independent cache with a monotonic clock.
    pub fn new() -> Self {
        let start = Instant::now();
        Self::with_clock(Arc::new(move || start.elapsed()))
    }
    /// Injects a monotonic elapsed-time source. It must not move backwards.
    pub fn with_clock(now: Arc<dyn Fn() -> Duration + Send + Sync>) -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            now,
        }
    }
    pub(crate) async fn fetch(
        &self,
        base: &str,
        auth: &ProviderAuth,
        account: &str,
    ) -> (Models, bool) {
        let key = Key::new(base, auth, account);
        loop {
            let flight = {
                let mut r = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                let now = (self.now)();
                r.completed
                    .retain(|_, c| now.saturating_sub(c.completed) < TTL);
                if let Some(c) = r.completed.get(&key) {
                    return (Ok(c.models.clone()), true);
                }
                r.flights.entry(key.clone()).or_default().clone()
            };
            loop {
                // Register before looking at state to avoid lost notify_waiters wakeups.
                let changed = flight.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let owner = {
                    let mut state = flight.state.lock().unwrap_or_else(|e| e.into_inner());
                    match &*state {
                        FlightState::Complete(models) => return (models.clone(), true),
                        FlightState::Abandoned => break,
                        FlightState::Owned => false,
                        FlightState::Available => {
                            *state = FlightState::Owned;
                            true
                        }
                    }
                };
                if owner {
                    let mut guard = Owner {
                        cache: self,
                        key: &key,
                        flight: &flight,
                        finished: false,
                    };
                    let result = fetch_codex_models(&key.url, auth, account)
                        .await
                        .map_err(|error| redact_error(error, &auth.api_key));
                    {
                        let mut r = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                        if remove_flight(&mut r, &key, &flight)
                            && let Ok(models) = &result
                            && !models.is_empty()
                        {
                            r.order += 1;
                            let order = r.order;
                            r.completed.insert(
                                key.clone(),
                                Cached {
                                    models: models.clone(),
                                    completed: (self.now)(),
                                    order,
                                },
                            );
                            while r.completed.len() > CAPACITY {
                                let oldest = r
                                    .completed
                                    .iter()
                                    .min_by_key(|(_, v)| v.order)
                                    .map(|(k, _)| k.clone())
                                    .expect("nonempty cache");
                                r.completed.remove(&oldest);
                            }
                        }
                        *flight.state.lock().unwrap_or_else(|e| e.into_inner()) =
                            FlightState::Complete(result.clone());
                        guard.finished = true;
                    }
                    flight.changed.notify_waiters();
                    return (result, false);
                }
                changed.await;
            }
        }
    }
}
// A remote error can reflect credentials. Never retain their raw bytes in a flight.
fn redact_error(mut error: ProviderError, token: &str) -> ProviderError {
    if token.is_empty() {
        return error;
    }
    match &mut error {
        ProviderError::Http { body, .. }
        | ProviderError::InvalidJson { detail: body }
        | ProviderError::InvalidSse { detail: body }
        | ProviderError::Request(body)
        | ProviderError::Transport { message: body } => {
            *body = body.replace(token, "***");
        }
        ProviderError::RetriesExhausted { last, .. } => {
            **last = redact_error((**last).clone(), token);
        }
        _ => {}
    }
    error
}

fn remove_flight(r: &mut Registry, key: &Key, flight: &Arc<Flight>) -> bool {
    if r.flights
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, flight))
    {
        r.flights.remove(key);
        true
    } else {
        false
    }
}
struct Owner<'a> {
    cache: &'a CodexCatalogCache,
    key: &'a Key,
    flight: &'a Arc<Flight>,
    finished: bool,
}
impl Drop for Owner<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut r = self
            .cache
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        remove_flight(&mut r, self.key, self.flight);
        *self.flight.state.lock().unwrap_or_else(|e| e.into_inner()) = FlightState::Abandoned;
        self.flight.changed.notify_waiters();
    }
}
/// Production instance shared by clients, routing, metadata and settings Fetch.
/// Getting it does not inspect or clear any saved state.
pub fn default_codex_catalog_cache() -> Arc<CodexCatalogCache> {
    static CACHE: OnceLock<Arc<CodexCatalogCache>> = OnceLock::new();
    Arc::clone(CACHE.get_or_init(|| Arc::new(CodexCatalogCache::new())))
}
#[cfg(test)]
mod tests;
