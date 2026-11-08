//! Pure eligibility and in-memory publishing attribution. No push ledger is persisted.
use super::super::diff::{self, ChecksVerdict};
use std::collections::HashMap;
use storage::pull_requests::{PullRequestFacts, PullRequestState};

pub(super) fn eligible_facts(facts: &PullRequestFacts, auto: bool) -> bool {
    facts.state == PullRequestState::Open
        && diff::checks_verdict(facts) == ChecksVerdict::Failing
        && (!auto || !diff::is_pending(Some(facts)))
}

#[derive(Default)]
pub(super) struct Attribution {
    threads: HashMap<String, Publishing>,
    baseline_links: std::collections::HashSet<(String, u64)>,
}
#[derive(Default)]
struct Publishing {
    generation: u64,
    success: u64,
    consumed: u64,
    interval: Option<Interval>,
}
struct Interval {
    revision: u64,
    head: String,
    seq: u64,
    agent: bool,
}
impl Attribution {
    fn thread(&mut self, thread: &str, generation: u64) -> &mut Publishing {
        let entry = self.threads.entry(thread.to_owned()).or_default();
        if entry.generation != generation {
            *entry = Publishing {
                generation,
                ..Default::default()
            };
        }
        entry
    }
    pub(super) fn linked(&mut self, thread: &str, generation: u64, revision: u64) {
        self.thread(thread, generation).interval = None;
        self.baseline_links.retain(|(id, _)| id != thread);
        self.baseline_links.insert((thread.to_owned(), revision));
    }
    pub(super) fn seed(
        &mut self,
        thread: &str,
        generation: u64,
        revision: u64,
        seq: u64,
        head: &str,
    ) {
        if self.baseline_links.contains(&(thread.to_owned(), revision)) {
            return;
        }
        let entry = self.thread(thread, generation);
        if entry.interval.is_none() {
            entry.consumed = entry.success;
            entry.interval = Some(Interval {
                revision,
                seq,
                head: head.to_owned(),
                agent: false,
            });
        }
    }
    pub(super) fn succeeded(&mut self, thread: &str, generation: u64) {
        let entry = self.thread(thread, generation);
        entry.success = entry.success.saturating_add(1);
    }
    /// Called only for an accepted fetch, with that fetch's fresh checkout resolution.
    /// Same-head polls retain classification and do not consume the next publishing signal.
    pub(super) fn adopt(
        &mut self,
        thread: &str,
        generation: u64,
        revision: u64,
        seq: u64,
        head: &str,
        local: Option<&str>,
    ) {
        self.baseline_links.remove(&(thread.to_owned(), revision));
        let entry = self.thread(thread, generation);
        if let Some(interval) = &mut entry.interval {
            if interval.revision == revision && interval.seq >= seq {
                return;
            }
            if interval.revision == revision && interval.head == head {
                interval.seq = seq;
                return;
            }
        }
        let agent = entry.success > entry.consumed && local == Some(head);
        entry.consumed = entry.success;
        entry.interval = Some(Interval {
            revision,
            head: head.to_owned(),
            seq,
            agent,
        });
    }
    pub(super) fn agent(
        &self,
        thread: &str,
        generation: u64,
        revision: u64,
        seq: u64,
        head: &str,
    ) -> bool {
        self.threads.get(thread).is_some_and(|p| {
            p.generation == generation
                && p.interval.as_ref().is_some_and(|i| {
                    i.revision == revision && i.seq == seq && i.head == head && i.agent
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{facts, failing, running};
    use super::*;
    #[test]
    fn failing_display_is_not_auto_eligibility() {
        let mut f = facts(PullRequestState::Open, failing());
        assert!(eligible_facts(&f, true));
        f.checks.extend(running());
        assert!(eligible_facts(&f, false));
        assert!(!eligible_facts(&f, true));
        f.checks = failing();
        f.checks_complete = false;
        assert!(!eligible_facts(&f, true));
        assert!(eligible_facts(&f, false));
        for state in [PullRequestState::Closed, PullRequestState::Merged] {
            f.state = state;
            assert!(!eligible_facts(&f, false));
        }
    }
    #[test]
    fn signal_is_consumed_per_head_not_per_poll_and_never_falls_back_to_saved_local() {
        let mut a = Attribution::default();
        a.succeeded("t", 1);
        a.adopt("t", 1, 1, 1, "a", Some("a"));
        assert!(a.agent("t", 1, 1, 1, "a"));
        // Pending -> complete keeps attribution even if the checkout disappears.
        a.adopt("t", 1, 1, 2, "a", None);
        assert!(a.agent("t", 1, 1, 2, "a"));
        a.adopt("t", 1, 1, 3, "b", Some("b"));
        assert!(!a.agent("t", 1, 1, 3, "b"));
        a.succeeded("t", 1);
        a.adopt("t", 1, 1, 4, "c", None);
        assert!(!a.agent("t", 1, 1, 4, "c"));
        // Resolving it on a later same-head poll cannot rewrite unknown attribution.
        a.adopt("t", 1, 1, 5, "c", Some("c"));
        assert!(!a.agent("t", 1, 1, 5, "c"));
    }
    #[test]
    fn link_relaxation_needs_unconsumed_success_and_a_fresh_matching_head() {
        for local in [None, Some("other"), Some("head")] {
            for success in [false, true] {
                let mut a = Attribution::default();
                if success {
                    a.succeeded("t", 1);
                }
                a.linked("t", 1, 3);
                a.seed("t", 1, 3, 1, "head");
                a.adopt("t", 1, 3, 2, "head", local);
                assert_eq!(
                    a.agent("t", 1, 3, 2, "head"),
                    success && local == Some("head")
                );
            }
        }
    }
    #[test]
    fn restart_and_owner_change_start_unknown_and_older_adoptions_cannot_replace_newer() {
        let mut a = Attribution::default();
        a.seed("t", 1, 1, 5, "a");
        a.succeeded("t", 1);
        a.adopt("t", 1, 1, 6, "a", Some("a"));
        assert!(!a.agent("t", 1, 1, 6, "a"));
        a.adopt("t", 1, 1, 7, "b", Some("b"));
        assert!(a.agent("t", 1, 1, 7, "b"));
        a.adopt("t", 1, 1, 6, "a", Some("a"));
        assert!(a.agent("t", 1, 1, 7, "b"));
        a.seed("t", 2, 1, 7, "b");
        assert!(!a.agent("t", 2, 1, 7, "b"));
    }
}
