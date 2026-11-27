//! The cost of the selected thread (ADR 0045 E4), shown in the conversation's header
//! (the whole thread, subagents included) and status line (the main agent only).
//!
//! The core answers it as a read operation of the usage ledger. The client keeps one
//! read for the selected thread, and the core's `usage_changed` event only says that
//! asking again may give another answer: a notice that arrives while the read waits
//! is asked once after it settled, never sooner than [`REASK_GAP`] after the last ask,
//! and only when it names the selected thread (or a run the client does not know).
use std::time::{Duration, Instant};

use workbench_protocol::{
    ids::{RunId, ThreadId},
    namespaces::diagnostics::{GetThreadUsage, ThreadUsage, UsageChange, UsageValue},
};

use super::read::{Phase, Read};
use super::usage::cells;
use crate::core_client::cache::Cache;
use crate::model::telemetry::pricing::PARTIAL_COST_HINT;

/// The shortest time between two asks made because of notices.
pub(super) const REASK_GAP: Duration = Duration::from_secs(2);

/// Runs noticed and not yet matched to the cache; more than this counts as every run.
const MAX_NOTICED_RUNS: usize = 256;

const RETENTION_HINT: &str =
    "Usage older than the retention period may be missing, so the cost is a lower bound.";

#[derive(Default)]
pub(super) struct ThreadUsageState {
    thread: Option<ThreadId>,
    read: Read,
    request: Option<GetThreadUsage>,
    data: Option<ThreadUsage>,
    /// The thread was never asked, or something may have changed since.
    dirty: bool,
    asked_at: Option<Instant>,
    noticed: Vec<RunId>,
    noticed_all: bool,
}

impl ThreadUsageState {
    pub(super) fn generation(&self) -> u64 {
        self.read.generation()
    }

    pub(super) fn read(&mut self) -> &mut Read {
        &mut self.read
    }

    /// The usage of `thread`, when it is the thread this state read.
    pub(super) fn usage(&self, thread: &ThreadId) -> Option<&ThreadUsage> {
        self.data.as_ref().filter(|data| &data.thread == thread)
    }

    pub(super) fn forget(&mut self) {
        self.read.forget();
        self.request = None;
        self.data = None;
        self.asked_at = None;
        self.noticed.clear();
        self.noticed_all = false;
        self.dirty = self.thread.is_some();
    }

    /// Follow the selection: another thread starts over, so its cost is never
    /// shown under the earlier thread's.
    fn follow(&mut self, thread: Option<&ThreadId>) {
        if self.thread.as_ref() != thread {
            self.thread = thread.cloned();
            self.forget();
        }
    }

    pub(super) fn notice(&mut self, change: &UsageChange) {
        if change.all_runs || self.noticed.len() + change.runs.len() > MAX_NOTICED_RUNS {
            self.noticed_all = true;
            self.noticed.clear();
        } else if !self.noticed_all {
            self.noticed.extend(change.runs.iter().cloned());
        }
    }

    /// Whether the notices so far may concern the thread: every run of it, and every
    /// run the cache does not know yet, since that run may be new in this thread.
    fn absorb(&mut self, cache: &Cache) {
        let Some(thread) = &self.thread else {
            self.noticed.clear();
            self.noticed_all = false;
            return;
        };
        let concerned = std::mem::take(&mut self.noticed_all)
            || self.noticed.iter().any(|run| {
                cache
                    .runs
                    .get(run.as_str())
                    .is_none_or(|summary| &summary.thread == thread)
            });
        self.noticed.clear();
        self.dirty |= concerned;
    }

    /// The request to send now, if any: a new read of the selected thread, or the
    /// settled one asked again for its data. `active` is whether the cost is wanted.
    pub(super) fn due(
        &mut self,
        selected: Option<&ThreadId>,
        cache: &Cache,
        now: Instant,
        active: bool,
    ) -> Option<GetThreadUsage> {
        self.follow(selected);
        self.absorb(cache);
        let thread = self.thread.clone()?;
        if !active {
            return None;
        }
        if *self.read.phase() == Phase::Waiting {
            return self
                .read
                .take_ask_again()
                .then(|| self.request.clone())
                .flatten();
        }
        // A thread not in the cache is a new conversation the core has not seen yet.
        if !self.dirty || !cache.threads.contains_key(thread.as_str()) {
            return None;
        }
        if self.asked_at.is_some_and(|at| now < at + REASK_GAP) {
            return None;
        }
        self.dirty = false;
        self.asked_at = Some(now);
        let request = GetThreadUsage {
            operation_id: self.read.begin("diagnostics-thread-usage"),
            thread,
        };
        self.request = Some(request.clone());
        Some(request)
    }

    /// How long until a notice may be asked, so the screen wakes for it.
    pub(super) fn wake_in(&self, now: Instant) -> Option<Duration> {
        if !self.dirty || self.thread.is_none() || *self.read.phase() == Phase::Waiting {
            return None;
        }
        self.asked_at
            .map(|at| (at + REASK_GAP).saturating_duration_since(now))
            .filter(|wait| !wait.is_zero())
    }

    pub(super) fn accept(&mut self, data: ThreadUsage) {
        self.data = Some(data);
    }
}

/// A cost as the header and the status line show it: `$1.50`, with `*` when it is a
/// lower bound, and `$—` when nothing was recorded or the cost is not known.
pub(crate) fn cost_text(value: Option<&UsageValue>) -> String {
    match value {
        Some(value) if value.totals.requests > 0 || value.incomplete => {
            let cells = cells(&value.totals);
            crate::format::cost_label(cells.cost, cells.lower_bound || value.incomplete)
        }
        _ => "$—".to_owned(),
    }
}

/// What a tooltip adds under `scope` when the cost is a lower bound.
pub(crate) fn cost_hint(scope: &str, value: Option<&UsageValue>) -> String {
    let mut hint = format!("{scope}: {}", cost_text(value));
    if let Some(value) = value {
        let cells = cells(&value.totals);
        if cells.lower_bound {
            hint = format!("{hint}\n{PARTIAL_COST_HINT}");
        }
        if value.incomplete {
            hint = format!("{hint}\n{RETENTION_HINT}");
        }
    }
    hint
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::namespaces::diagnostics::UsageTotals;

    fn value(requests: u64, micro: u64, unpriced: u64, incomplete: bool) -> UsageValue {
        UsageValue {
            totals: UsageTotals {
                requests,
                recorded_cost_micro_usd: micro,
                unpriced_tokens: unpriced,
                ..Default::default()
            },
            incomplete,
        }
    }

    #[test]
    fn a_cost_is_marked_a_lower_bound_and_nothing_recorded_is_a_dash() {
        assert_eq!(cost_text(None), "$—");
        assert_eq!(cost_text(Some(&value(0, 0, 0, false))), "$—");
        assert_eq!(cost_text(Some(&value(2, 1_500_000, 0, false))), "$1.50");
        assert_eq!(cost_text(Some(&value(2, 1_500_000, 9, false))), "$1.50*");
        assert_eq!(cost_text(Some(&value(0, 0, 0, true))), "$0.00*");
        assert_eq!(cost_text(Some(&value(2, 1_500_000, 0, true))), "$1.50*");
    }

    #[test]
    fn the_tooltip_explains_why_a_cost_is_a_lower_bound() {
        let plain = cost_hint("Scope", Some(&value(1, 1_000_000, 0, false)));
        assert_eq!(plain, "Scope: $1.00");
        let partial = cost_hint("Scope", Some(&value(1, 1_000_000, 5, false)));
        assert!(partial.contains(PARTIAL_COST_HINT));
        let rolled = cost_hint("Scope", Some(&value(1, 1_000_000, 0, true)));
        assert!(rolled.contains(RETENTION_HINT));
    }
}
