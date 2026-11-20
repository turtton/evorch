//! State and deadline are published in one CAS, without an intermediate Arming.
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const DORMANT: u64 = 0;
pub(super) const COMPLETED: u64 = u64::MAX - 1;
pub(super) const EXPIRED: u64 = u64::MAX;
pub(super) const BUDGET_NS: u64 = 10_000_000_000;

pub(super) struct Deadline(AtomicU64);
impl Deadline {
    pub(super) fn new() -> Self {
        Self(AtomicU64::new(DORMANT))
    }
    pub(super) fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
    pub(super) fn arm(&self, now: u64) {
        // Overflow fails closed: 1 is already due, never Dormant/a sentinel.
        let deadline = now
            .checked_add(BUDGET_NS)
            .filter(|d| *d < COMPLETED)
            .unwrap_or(1);
        let _ = self
            .0
            .compare_exchange(DORMANT, deadline, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(super) fn expire(&self, expected: u64, now: u64) -> bool {
        expected != DORMANT
            && expected < COMPLETED
            && now >= expected
            && self
                .0
                .compare_exchange(expected, EXPIRED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
    pub(super) fn complete(&self, now: u64) -> bool {
        let mut current = self.get();
        loop {
            if current == COMPLETED {
                return true;
            }
            if current == EXPIRED || (current != DORMANT && now >= current) {
                return false;
            }
            match self
                .0
                .compare_exchange(current, COMPLETED, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
    }
}
