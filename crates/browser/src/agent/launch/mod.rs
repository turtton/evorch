//! Chromium owns its process; chromiumoxide only owns a CDP connection.
mod args;
mod endpoint;
mod owner;
use chromiumoxide::handler::HandlerConfig;
pub(super) use owner::Chromium;

pub(super) fn handler_config() -> HandlerConfig {
    // C-13: do not inherit HandlerConfig::default() (it ignores TLS errors).
    HandlerConfig {
        ignore_https_errors: false,
        ignore_invalid_messages: true,
        viewport: None,
        context_ids: Vec::new(),
        request_timeout: super::super::REQUEST_TIMEOUT,
        request_intercept: false,
        cache_enabled: true,
    }
}

/// Also protects the interval between spawning a driver and constructing Running.
pub(super) struct AbortTask(tokio::task::JoinHandle<()>);
impl AbortTask {
    pub(super) fn new(task: tokio::task::JoinHandle<()>) -> Self {
        Self(task)
    }
    pub(super) fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
