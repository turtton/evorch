//! Read-only, core-local evidence of actual worker heartbeat results.
use std::sync::{Arc, Mutex, mpsc};

use event_bus::EventBus;

use super::ThreadOwner;

/// Explicitly installed by recovery fixtures; never an ownership authority.
pub struct HeartbeatObservations {
    sender: mpsc::Sender<ThreadOwner>,
    rows: Mutex<mpsc::Receiver<ThreadOwner>>,
}

impl HeartbeatObservations {
    pub fn install_for_test(bus: &EventBus) -> Arc<Self> {
        bus.shared_state(|| {
            let (sender, receiver) = mpsc::channel();
            Self {
                sender,
                rows: Mutex::new(receiver),
            }
        })
    }

    /// Copies recorded results, not current rows re-read from the registry.
    pub fn drain(&self) -> Vec<ThreadOwner> {
        self.rows.lock().unwrap().try_iter().collect()
    }
}

pub(super) fn record(bus: &EventBus, owner: &ThreadOwner) {
    if let Some(observations) = bus.get_shared_state::<HeartbeatObservations>() {
        // Never block on the consumer or propagate an observer failure into the
        // worker. Recording neither re-reads nor changes the committed row.
        let _ = observations.sender.send(owner.clone());
    }
}
