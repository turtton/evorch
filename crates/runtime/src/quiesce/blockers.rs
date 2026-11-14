//! Temporary safety exclusions are not storage-health failures.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartBlockerKind {
    InFlightAdmission,
    AcceptedResumePending,
    AwaitingSafePoint,
    ShellRunning,
    ShellResultUnobserved,
    UnconfirmedRunJs,
    UnfinishedToolBatch,
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
    /// Only bounded identities and categories, never tool arguments or output.
    pub fn restart_blockers(&self) -> Vec<RestartBlocker> {
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
                inner
                    .blockers
                    .get(&run)
                    .filter(|b| {
                        b.generation == inner.generation && b.incarnation == intent.incarnation
                    })
                    .cloned()
                    .or_else(|| {
                        Some(RestartBlocker {
                            run,
                            incarnation: intent.incarnation.clone(),
                            generation: inner.generation,
                            kind: if intent.wait == ResumeWait::Admission {
                                RestartBlockerKind::InFlightAdmission
                            } else {
                                RestartBlockerKind::AwaitingSafePoint
                            },
                        })
                    })
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
