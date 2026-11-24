//! Temporary safety exclusions are not storage-health failures.
use super::*;
use sandbox::approval::ApprovalSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartBlockerKind {
    InFlightAdmission,
    AcceptedResumePending,
    AwaitingSafePoint,
    ShellRunning,
    ShellResultUnobserved,
    UnconfirmedRunJs,
    UnfinishedToolBatch,
    ApprovalPending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartBlocker {
    pub run: RunId,
    pub incarnation: String,
    pub generation: u64,
    pub kind: RestartBlockerKind,
}

impl Coordinator {
    pub(crate) fn block(&self, run: RunId, incarnation: &str, kind: RestartBlockerKind) {
        let mut inner = self.lock();
        if !inner.requested
            || !inner
                .intents
                .get(&run)
                .is_some_and(|i| i.active && i.incarnation == incarnation)
        {
            return;
        }
        let blocker = RestartBlocker {
            run,
            incarnation: incarnation.into(),
            generation: inner.generation,
            kind,
        };
        if inner.blockers.get(&run) == Some(&blocker) {
            return;
        }
        inner.acks.remove(&run);
        inner.blockers.insert(run, blocker);
        if inner.phase == QuiesceState::Checkpointed {
            inner.phase = QuiesceState::WaitingForSafePoints;
        }
        self.changed();
    }

    pub(crate) fn clear_blocker(&self, run: RunId, incarnation: &str) {
        let mut inner = self.lock();
        if inner
            .blockers
            .get(&run)
            .is_some_and(|b| b.incarnation == incarnation)
        {
            inner.blockers.remove(&run);
            self.changed();
        }
    }
}

impl crate::AgentRuntime {
    pub fn pending_approvals(&self) -> Vec<ApprovalSnapshot> {
        use sandbox::approval::{ApprovalRegistry, PAGE_MAX};
        let Some(registry) = ApprovalRegistry::existing(self.event_bus().as_ref()) else {
            return Vec::new();
        };
        let mut pending: Vec<ApprovalSnapshot> = Vec::new();
        loop {
            let page = registry.list_pending(pending.last().map(|a| a.attempt), PAGE_MAX);
            let last = page.len() < PAGE_MAX;
            pending.extend(page);
            if last {
                return pending;
            }
        }
    }

    /// Only bounded identities and categories, never tool arguments or output.
    pub fn restart_blockers(&self) -> Vec<RestartBlocker> {
        let approvals: std::collections::BTreeSet<String> = self
            .pending_approvals()
            .into_iter()
            .map(|approval| approval.run_id)
            .collect();
        let accepted = self.accepted_chat_resumes();
        let inner = self.shared.quiesce.lock();
        if !inner.requested {
            return Vec::new();
        }
        inner
            .intents
            .values()
            .filter_map(|intent| {
                let run = intent.run_id;
                if accepted.contains_key(&run)
                    || !intent.active
                    || inner.unstarted.contains(&run)
                    || intent.wait == ResumeWait::AdmissionQueued
                {
                    return None;
                }
                let acknowledged =
                    inner
                        .acks
                        .get(&run)
                        .is_some_and(|(generation, incarnation, revision)| {
                            *generation == inner.generation
                                && *incarnation == intent.incarnation
                                && *revision == *inner.revisions.get(&run).unwrap_or(&0)
                        });
                if acknowledged {
                    return None;
                }
                let mut blocker = inner
                    .blockers
                    .get(&run)
                    .filter(|b| {
                        b.generation == inner.generation && b.incarnation == intent.incarnation
                    })
                    .cloned()
                    .unwrap_or_else(|| RestartBlocker {
                        run,
                        incarnation: intent.incarnation.clone(),
                        generation: inner.generation,
                        kind: if intent.wait == ResumeWait::Admission {
                            RestartBlockerKind::InFlightAdmission
                        } else {
                            RestartBlockerKind::AwaitingSafePoint
                        },
                    });
                if approvals.contains(&run.to_string()) {
                    blocker.kind = RestartBlockerKind::ApprovalPending;
                }
                Some(blocker)
            })
            .chain(accepted.iter().map(|(run, incarnation)| RestartBlocker {
                run: *run,
                incarnation: incarnation.clone(),
                generation: inner.generation,
                kind: RestartBlockerKind::AcceptedResumePending,
            }))
            .collect()
    }
}
