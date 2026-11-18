use super::*;
use crate::lifecycle::idle_clock::Clock;

pub(super) struct ManualClock(tokio::sync::watch::Sender<Instant>);
impl ManualClock {
    pub(super) fn new(start: Instant) -> Self {
        Self(tokio::sync::watch::channel(start).0)
    }
    pub(super) fn advance(&self, by: Duration) {
        let next = *self.0.borrow() + by;
        self.0.send_replace(next);
    }
}
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.0.borrow()
    }
    async fn expired(&self, deadline: Option<Instant>) {
        let Some(deadline) = deadline else {
            return std::future::pending().await;
        };
        let mut observed = self.0.subscribe();
        loop {
            if *observed.borrow_and_update() >= deadline {
                return;
            }
            observed.changed().await.unwrap();
        }
    }
}
