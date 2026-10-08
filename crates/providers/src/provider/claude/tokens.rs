use crate::ProviderError;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{Mutex, PoisonError},
};

/// Persist the whole rotated token family atomically in the OS credential store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeTokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    /// UNIX epoch seconds, without a pre-subtracted refresh skew.
    pub expires_at: u64,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
}
impl ClaudeTokenBundle {
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at.saturating_sub(now) <= 300
    }
}
impl fmt::Debug for ClaudeTokenBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeTokenBundle")
            .field("credentials", &"<redacted>")
            .finish()
    }
}
pub trait ClaudeTokenStore: Send + Sync {
    fn load(&self) -> Result<Option<ClaudeTokenBundle>, ProviderError>;
    fn save(&self, bundle: &ClaudeTokenBundle) -> Result<(), ProviderError>;
    /// Implementations may return a per-account lock. Default safely serializes all accounts.
    fn refresh_lock(&self) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        static LOCK: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}
#[derive(Default)]
pub struct InMemoryClaudeTokenStore {
    bundle: Mutex<Option<ClaudeTokenBundle>>,
}
impl InMemoryClaudeTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}
impl ClaudeTokenStore for InMemoryClaudeTokenStore {
    fn load(&self) -> Result<Option<ClaudeTokenBundle>, ProviderError> {
        Ok(self
            .bundle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
    fn save(&self, bundle: &ClaudeTokenBundle) -> Result<(), ProviderError> {
        *self.bundle.lock().unwrap_or_else(PoisonError::into_inner) = Some(bundle.clone());
        Ok(())
    }
}
