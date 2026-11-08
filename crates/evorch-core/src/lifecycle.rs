//! Idle time is measured only while all three retention conditions are false.
use std::time::Duration;
use tokio::time::Instant;

pub struct IdleGrace {
    grace: Duration,
    since: Option<Instant>,
}
impl IdleGrace {
    pub fn new(grace: Duration) -> Self {
        Self { grace, since: None }
    }
    pub fn update(&mut self, web_enabled: bool, clients: usize, busy: bool) -> Option<Instant> {
        if web_enabled || clients != 0 || busy {
            self.since = None;
            return None;
        }
        self.since
            .get_or_insert_with(Instant::now)
            .checked_add(self.grace)
    }
}
