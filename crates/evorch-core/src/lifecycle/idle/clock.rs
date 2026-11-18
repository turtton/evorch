use std::future::Future;
use tokio::time::Instant;

/// One clock supplies both policy observations and expiry readiness. Production
/// uses Tokio; composition tests inject watch-driven time on a multithread runtime.
pub(super) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn expired(&self, deadline: Option<Instant>) -> impl Future<Output = ()> + Send;
}

pub(super) struct TokioClock;
impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    async fn expired(&self, deadline: Option<Instant>) {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }
}
