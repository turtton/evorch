//! When a subagent's transcript tab opens, comes back and is parked by itself, as the
//! embedded workbench does on its run lifecycle events. The core client has no such
//! events, only each run's phase in the projection, so the phases are compared once per
//! frame: a run seen starting opens its tab, one seen finishing parks it. The runs of the
//! first projection are new too, so a window opened on them gives each running subagent
//! a split and each finished one a parked tab, as the embedded workbench rebuilds its
//! subagent panes from history at startup.
use std::collections::BTreeMap;

use workbench_protocol::ids::{RunId, ThreadId};
use workbench_protocol::views::{RunPhase, RunSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The run started (or was started again): its tab opens, or leaves its parking.
    Start,
    /// The run works again after waiting on the person: a parked tab leaves its parking.
    Resume,
    /// The run ended: its tab is parked, and opened parked when it has none.
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub thread: ThreadId,
    pub run: RunId,
}

#[derive(Default)]
pub struct SubagentWatch {
    /// The phase each subagent had when last seen.
    seen: BTreeMap<String, RunPhase>,
}

fn finished(phase: RunPhase) -> bool {
    matches!(
        phase,
        RunPhase::Done | RunPhase::Failed | RunPhase::Cancelled
    )
}

fn waiting(phase: RunPhase) -> bool {
    matches!(
        phase,
        RunPhase::WaitingForUser | RunPhase::WaitingForApproval
    )
}

impl SubagentWatch {
    /// What changed for the subagents (runs with a parent) of `runs` since the last
    /// call. A subagent not seen before starts, or finishes when it already ended, the
    /// first call's too: a run with a tab already (one the layout file brought back) keeps
    /// it. What was seen is kept across new projections, so a run that ended while
    /// disconnected is parked.
    pub fn observe(&mut self, runs: &BTreeMap<String, RunSummary>) -> Vec<Change> {
        let seen = &mut self.seen;
        let mut changes = Vec::new();
        for (id, run) in runs.iter().filter(|(_, run)| run.parent_run.is_some()) {
            let before = seen.insert(id.clone(), run.phase);
            let kind = match (before, run.phase) {
                (Some(before), now) if before == now => continue,
                (None, now) if finished(now) => ChangeKind::Finish,
                (Some(before), now) if finished(now) && !finished(before) => ChangeKind::Finish,
                (None, _) => ChangeKind::Start,
                (Some(before), now) if finished(before) && !finished(now) => ChangeKind::Start,
                (Some(before), now) if waiting(before) && !waiting(now) => ChangeKind::Resume,
                _ => continue,
            };
            changes.push(Change {
                kind,
                thread: run.thread.clone(),
                run: run.id.clone(),
            });
        }
        seen.retain(|id, _| runs.contains_key(id));
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(list: &[(&str, Option<&str>, &str)]) -> BTreeMap<String, RunSummary> {
        list.iter()
            .map(|(id, parent, phase)| {
                let run: RunSummary = serde_json::from_value(serde_json::json!({
                    "id": id, "thread": "t1", "parent_run": parent, "role": "worker",
                    "phase": phase, "revision": "1",
                }))
                .unwrap();
                ((*id).to_owned(), run)
            })
            .collect()
    }

    fn kinds(changes: &[Change]) -> Vec<(&str, ChangeKind)> {
        changes
            .iter()
            .map(|change| (change.run.as_str(), change.kind))
            .collect()
    }

    // Given: a root and three subagents already there when the window opens
    // When: the phases change frame by frame
    // Then: the first frame starts the running and waiting subagents and finishes the
    //       ended one, as the embedded workbench restores its panes; then a new subagent
    //       starts, one that ends is finished, a waiting one that works again resumes, a
    //       finished one started again starts, a root changes nothing, and one seen first
    //       finished is finished
    #[test]
    fn subagent_phases_turn_into_starts_resumes_and_finishes() {
        use ChangeKind::*;
        let mut watch = SubagentWatch::default();
        let first = runs(&[
            ("root", None, "running"),
            ("a", Some("root"), "running"),
            ("b", Some("root"), "waiting_for_user"),
            ("e", Some("root"), "done"),
        ]);
        assert_eq!(
            kinds(&watch.observe(&first)),
            [("a", Start), ("b", Start), ("e", Finish)]
        );
        assert!(watch.observe(&first).is_empty(), "nothing changed");

        let next = runs(&[
            ("root", None, "done"),
            ("a", Some("root"), "done"),
            ("b", Some("root"), "running"),
            ("c", Some("a"), "pending"),
            ("d", Some("root"), "failed"),
        ]);
        assert_eq!(
            kinds(&watch.observe(&next)),
            [("a", Finish), ("b", Resume), ("c", Start), ("d", Finish)]
        );

        let again = runs(&[
            ("a", Some("root"), "running"),
            ("b", Some("root"), "waiting_for_approval"),
            ("c", Some("a"), "running"),
        ]);
        assert_eq!(kinds(&watch.observe(&again)), [("a", Start)]);
        // d left the projection: seen again later, it is a new run.
        let back = runs(&[("d", Some("root"), "running")]);
        assert_eq!(kinds(&watch.observe(&back)), [("d", Start)]);
    }
}
