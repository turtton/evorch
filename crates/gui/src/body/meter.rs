//! Counts the bodies read at once, by what needed them, and the most text one
//! event held while it was being turned into an event.
//!
//! A display body is read lazily and counted by the resolver. Everything else
//! a stored event names (a tool's input or detail, a shell result a job card
//! parses, the display body of an event another window wrote) is read while
//! the event is built, on whichever thread builds it: that is what this meter
//! sees. Nothing here is a limit; it only makes the figures visible.

use std::sync::{Arc, Mutex};

/// Why a body was read at once instead of when a pane showed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmediateClass {
    /// A token inside a tool's input or detail (or any other nested string).
    Nested,
    /// The result of a tool a job card parses (`shell`, `bash`).
    ControlResult,
    /// The display body of an event another window wrote: the feed's reader
    /// joins strings, so it reads the text on its worker.
    FeedDisplay,
    /// The text of an unfinished response carried over to the live assembler.
    PartHandOver,
}

/// Reads and bytes of one class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassStats {
    pub bodies: u64,
    pub bytes: u64,
}

/// Everything the meter counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImmediateStats {
    pub nested: ClassStats,
    pub control_result: ClassStats,
    pub feed_display: ClassStats,
    pub part_hand_over: ClassStats,
    /// The most text one event held in bodies read at once while it was being
    /// built. Held outside the resolver's cache, so outside its limit.
    pub largest_event_bytes: u64,
}

impl ImmediateStats {
    /// Bodies read at once, of every class.
    #[must_use]
    pub const fn total_bodies(&self) -> u64 {
        self.nested.bodies
            + self.control_result.bodies
            + self.feed_display.bodies
            + self.part_hand_over.bodies
    }

    #[must_use]
    pub const fn class(&self, class: ImmediateClass) -> ClassStats {
        match class {
            ImmediateClass::Nested => self.nested,
            ImmediateClass::ControlResult => self.control_result,
            ImmediateClass::FeedDisplay => self.feed_display,
            ImmediateClass::PartHandOver => self.part_hand_over,
        }
    }

    const fn class_mut(&mut self, class: ImmediateClass) -> &mut ClassStats {
        match class {
            ImmediateClass::Nested => &mut self.nested,
            ImmediateClass::ControlResult => &mut self.control_result,
            ImmediateClass::FeedDisplay => &mut self.feed_display,
            ImmediateClass::PartHandOver => &mut self.part_hand_over,
        }
    }
}

/// A shared counter: clones count into the same figures.
#[derive(Debug, Clone, Default)]
pub struct ImmediateMeter {
    stats: Arc<Mutex<ImmediateStats>>,
}

impl ImmediateMeter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ImmediateStats> {
        self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One body of `bytes` read at once for `class`.
    pub fn record(&self, class: ImmediateClass, bytes: usize) {
        let mut stats = self.lock();
        let slot = stats.class_mut(class);
        slot.bodies += 1;
        slot.bytes += bytes as u64;
    }

    /// An event was built holding `bytes` of text read at once.
    pub fn event_built(&self, bytes: u64) {
        let mut stats = self.lock();
        stats.largest_event_bytes = stats.largest_event_bytes.max(bytes);
    }

    #[must_use]
    pub fn snapshot(&self) -> ImmediateStats {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_count_together_and_classes_stay_apart() {
        let meter = ImmediateMeter::new();
        let other = meter.clone();
        meter.record(ImmediateClass::Nested, 10);
        other.record(ImmediateClass::Nested, 5);
        other.record(ImmediateClass::ControlResult, 7);
        meter.event_built(15);
        meter.event_built(3);
        let stats = other.snapshot();
        assert_eq!(
            stats.nested,
            ClassStats {
                bodies: 2,
                bytes: 15
            }
        );
        assert_eq!(
            stats.control_result,
            ClassStats {
                bodies: 1,
                bytes: 7
            }
        );
        assert_eq!(stats.feed_display, ClassStats::default());
        assert_eq!(
            stats.largest_event_bytes, 15,
            "the largest event, not the last"
        );
        assert_eq!(stats.total_bodies(), 3);
    }
}
