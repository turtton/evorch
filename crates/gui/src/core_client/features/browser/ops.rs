//! Renderer operations as the client sees them.
//!
//! A renderer command is answered with `Accepted { operation_id }`, which only
//! says the core took the request. The outcome is a separate operation the
//! client asks for (`GetRendererOperation`) and that settles later. This module
//! tracks each operation through those steps for the feature and owns three
//! rules:
//!
//! * `Accepted` is never a result. The operation is queried until it is
//!   `Completed`, `Failed` or `Cancelled`; an `OperationChanged` event (or a
//!   reset) is what asks for the next query, so nothing polls on a timer.
//! * A command whose reply was lost (`outcome_unknown`) is never sent again. Its
//!   resolved reply, delivered later for the same tag, continues it; when none
//!   comes it ends as [`Failure::Unknown`].
//! * A `Busy` answer is retried only as often as [`MAX_AUTO_RETRIES`], after a
//!   growing delay, and only for the advice that allows it; every other advice
//!   leaves the decision to the user.
//!
//! Time comes from a [`Clock`] so that tests move it by hand.
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use super::super::{FeatureError, FeatureReply, FeatureRequest};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::{ErrorCode, RetryAdvice},
    namespaces::{
        artifacts::ArtifactsResult,
        browser::{
            BrowserQuery, BrowserReply, BrowserResult, RendererFailure, RendererId,
            RendererOperationState, RendererOutput,
        },
    },
    queries::{Query, QueryReply},
};

/// How many times a `Busy` refusal is sent again by itself.
pub(super) const MAX_AUTO_RETRIES: u8 = 3;
const RETRY_BASE: Duration = Duration::from_millis(250);
/// How long an accepted or unresolved operation may wait for its result.
pub(super) const OP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct Clock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl Clock {
    pub fn system() -> Self {
        Self(Arc::new(Instant::now))
    }
    #[cfg(test)]
    pub fn from_fn(f: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn now(&self) -> Instant {
        (self.0)()
    }
}

pub(super) type OpId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// The command is to be sent at the next drive.
    Ready,
    /// Refused as busy; sent again at `until`.
    Backoff {
        until: Instant,
    },
    Sent {
        tag: u64,
    },
    /// The reply was lost. Waiting for the resolved reply of `tag`; never resent.
    Unknown {
        tag: u64,
    },
    NeedQuery {
        operation: RendererId,
    },
    Querying {
        operation: RendererId,
        tag: u64,
        seen: u64,
    },
    /// `Running`: the next `OperationChanged` event or reset asks again.
    Waiting {
        operation: RendererId,
    },
}

struct Op<P> {
    purpose: P,
    command: Command,
    stage: Stage,
    retries: u8,
    since: Instant,
    operation: Option<RendererId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Failure {
    /// The core answered the command or the query with a refusal.
    Refused(FeatureError),
    /// The operation itself failed or was cancelled.
    Renderer(RendererFailure),
    Unexpected(&'static str),
    /// The reply was lost and no resolved reply came.
    Unknown,
    /// Accepted, but the result never came.
    TimedOut,
}

impl Failure {
    /// What may be shown to the user. Never helper-provided text.
    pub fn describe(&self) -> String {
        match self {
            Self::Refused(error) => error.message.clone(),
            Self::Renderer(reason) => format!("renderer: {}", failure_label(*reason)),
            Self::Unexpected(what) => format!("unexpected answer: {what}"),
            Self::Unknown => "result unknown (not sent again)".to_owned(),
            Self::TimedOut => "no result came".to_owned(),
        }
    }
}

pub(super) fn failure_label(reason: RendererFailure) -> &'static str {
    match reason {
        RendererFailure::Unavailable => "unavailable",
        RendererFailure::Busy => "busy",
        RendererFailure::StaleGeneration => "stale generation",
        RendererFailure::FrameTooLarge => "frame too large",
        RendererFailure::ResultTooLarge => "result too large",
        RendererFailure::InvalidRequest => "invalid request",
        RendererFailure::TimedOut => "timed out",
        RendererFailure::Restart => "core restarting",
        RendererFailure::Closed => "closed",
    }
}

#[derive(Debug)]
pub(super) struct Finished<P> {
    pub id: OpId,
    pub purpose: P,
    /// The operation id the core assigned, when it got that far.
    pub operation: Option<RendererId>,
    pub result: Result<RendererOutput, Failure>,
}

pub(super) struct Ops<P> {
    next_op: OpId,
    next_tag: u64,
    event_seq: u64,
    ops: BTreeMap<OpId, Op<P>>,
    /// tag -> op, for the replies still expected.
    tags: BTreeMap<u64, OpId>,
}

impl<P> Default for Ops<P> {
    fn default() -> Self {
        Self {
            next_op: 0,
            next_tag: 0,
            event_seq: 0,
            ops: BTreeMap::new(),
            tags: BTreeMap::new(),
        }
    }
}

fn operation_of(command_result: &CommandResult) -> Option<RendererId> {
    match command_result {
        CommandResult::Browser(BrowserResult::Accepted { operation_id })
        | CommandResult::Artifacts(ArtifactsResult::Accepted { operation_id }) => {
            Some(operation_id.clone())
        }
        _ => None,
    }
}

fn backoff(retries: u8) -> Duration {
    RETRY_BASE * 2u32.pow(u32::from(retries))
}

/// Whether a refusal is sent again by itself: `Busy` with the advice that says
/// the same request may be repeated, and only a few times.
fn retryable(error: &FeatureError, retries: u8) -> bool {
    error.code == Some(ErrorCode::Busy)
        && error.retry == Some(RetryAdvice::SameRequest)
        && retries < MAX_AUTO_RETRIES
}

impl<P: Clone> Ops<P> {
    pub fn submit(&mut self, purpose: P, command: Command, now: Instant) -> OpId {
        self.next_op += 1;
        self.ops.insert(
            self.next_op,
            Op {
                purpose,
                command,
                stage: Stage::Ready,
                retries: 0,
                since: now,
                operation: None,
            },
        );
        self.next_op
    }

    pub fn contains(&self, id: OpId) -> bool {
        self.ops.contains_key(&id)
    }

    /// The reply of a command is lost: shown as unknown.
    pub fn is_unknown(&self, id: OpId) -> bool {
        self.ops
            .get(&id)
            .is_some_and(|op| matches!(op.stage, Stage::Unknown { .. }))
    }

    /// Forget an operation. A late reply for it is ignored.
    pub fn abandon(&mut self, id: OpId) -> Option<P> {
        self.tags.retain(|_, op| *op != id);
        self.ops.remove(&id).map(|op| op.purpose)
    }

    /// Forget every operation whose purpose `pred` selects.
    pub fn abandon_where(&mut self, pred: impl Fn(&P) -> bool) -> Vec<P> {
        let ids: Vec<_> = self
            .ops
            .iter()
            .filter(|(_, op)| pred(&op.purpose))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().filter_map(|id| self.abandon(id)).collect()
    }

    /// An `OperationChanged` event: whatever waits is asked again, and so is
    /// whatever was asked before the event.
    pub fn on_event(&mut self) {
        self.event_seq += 1;
        for op in self.ops.values_mut() {
            if let Stage::Waiting { operation } = &op.stage {
                op.stage = Stage::NeedQuery {
                    operation: operation.clone(),
                };
            }
        }
    }

    /// A new snapshot replaced the projection. Operations whose result was
    /// being waited for are asked once more; commands are not touched (a lost
    /// reply is resolved by the receipt, never by a resend).
    pub fn on_reset(&mut self, now: Instant) {
        for op in self.ops.values_mut() {
            let operation = match &op.stage {
                Stage::Querying { operation, .. } | Stage::Waiting { operation } => {
                    Some(operation.clone())
                }
                _ => None,
            };
            if let Some(operation) = operation {
                op.stage = Stage::NeedQuery { operation };
                op.since = now;
            }
        }
        self.tags.retain(|_, id| {
            self.ops
                .get(id)
                .is_some_and(|op| matches!(op.stage, Stage::Sent { .. } | Stage::Unknown { .. }))
        });
    }

    fn tag(&mut self, id: OpId) -> u64 {
        self.next_tag += 1;
        self.tags.insert(self.next_tag, id);
        self.next_tag
    }

    /// Send what is due, end what ran out of time, and say when to look again.
    /// What `hold` selects is not sent or asked for now (a session being
    /// resumed answers nothing about its operations yet); it keeps its state.
    pub fn drive(
        &mut self,
        now: Instant,
        hold: &dyn Fn(&P) -> bool,
        send: &mut dyn FnMut(u64, FeatureRequest),
    ) -> (Vec<Finished<P>>, Option<Instant>) {
        let mut finished = Vec::new();
        let mut wake: Option<Instant> = None;
        let mut soon = |at: Instant| wake = Some(wake.map_or(at, |w| w.min(at)));
        let ids: Vec<OpId> = self.ops.keys().copied().collect();
        for id in ids {
            let stage = self.ops[&id].stage.clone();
            if matches!(
                stage,
                Stage::Ready | Stage::Backoff { .. } | Stage::NeedQuery { .. }
            ) && hold(&self.ops[&id].purpose)
            {
                continue;
            }
            match stage {
                Stage::Ready => {
                    let tag = self.tag(id);
                    let op = self.ops.get_mut(&id).unwrap();
                    op.stage = Stage::Sent { tag };
                    send(tag, FeatureRequest::Command(op.command.clone()));
                }
                Stage::Backoff { until } => {
                    if until <= now {
                        let tag = self.tag(id);
                        let op = self.ops.get_mut(&id).unwrap();
                        op.stage = Stage::Sent { tag };
                        send(tag, FeatureRequest::Command(op.command.clone()));
                    } else {
                        soon(until);
                    }
                }
                Stage::NeedQuery { operation } => {
                    let tag = self.tag(id);
                    let seen = self.event_seq;
                    let op = self.ops.get_mut(&id).unwrap();
                    op.stage = Stage::Querying {
                        operation: operation.clone(),
                        tag,
                        seen,
                    };
                    send(
                        tag,
                        FeatureRequest::Query(Query::Browser(BrowserQuery::GetRendererOperation {
                            operation_id: operation,
                        })),
                    );
                }
                Stage::Sent { .. }
                | Stage::Unknown { .. }
                | Stage::Querying { .. }
                | Stage::Waiting { .. } => {}
            }
            let op = &self.ops[&id];
            let waiting = matches!(
                op.stage,
                Stage::Unknown { .. } | Stage::Querying { .. } | Stage::Waiting { .. }
            );
            if waiting {
                let deadline = op.since + OP_TIMEOUT;
                if deadline <= now {
                    let failure = if matches!(op.stage, Stage::Unknown { .. }) {
                        Failure::Unknown
                    } else {
                        Failure::TimedOut
                    };
                    finished.push(self.finish(id, Err(failure)));
                } else {
                    soon(deadline);
                }
            }
        }
        (finished, wake)
    }

    fn finish(&mut self, id: OpId, result: Result<RendererOutput, Failure>) -> Finished<P> {
        self.tags.retain(|_, op| *op != id);
        let op = self.ops.remove(&id).expect("a tracked operation");
        Finished {
            id,
            purpose: op.purpose,
            operation: op.operation,
            result,
        }
    }

    fn retry(&mut self, id: OpId, now: Instant) {
        let op = self.ops.get_mut(&id).unwrap();
        op.stage = Stage::Backoff {
            until: now + backoff(op.retries),
        };
        op.retries += 1;
        op.operation = None;
        self.tags.retain(|_, owner| *owner != id);
    }

    /// The reply of a request this module sent. `None`: it continues, or it
    /// was not one of ours.
    pub fn on_reply(&mut self, tag: u64, reply: FeatureReply, now: Instant) -> Option<Finished<P>> {
        let id = *self.tags.get(&tag)?;
        let stage = self.ops.get(&id)?.stage.clone();
        match (stage, reply) {
            (Stage::Sent { tag: t } | Stage::Unknown { tag: t }, FeatureReply::Command(result))
                if t == tag =>
            {
                match result {
                    Ok(reply) => {
                        let Some(operation) = reply.result.as_deref().and_then(operation_of) else {
                            return Some(self.finish(
                                id,
                                Err(Failure::Unexpected(
                                    "the command was not accepted as an operation",
                                )),
                            ));
                        };
                        let op = self.ops.get_mut(&id).unwrap();
                        op.operation = Some(operation.clone());
                        op.since = now;
                        op.stage = Stage::NeedQuery { operation };
                        self.tags.remove(&tag);
                        None
                    }
                    Err(error) if error.outcome_unknown => {
                        let op = self.ops.get_mut(&id).unwrap();
                        if matches!(op.stage, Stage::Sent { .. }) {
                            op.stage = Stage::Unknown { tag };
                            op.since = now;
                        }
                        None
                    }
                    Err(error) => {
                        let retries = self.ops[&id].retries;
                        if retryable(&error, retries) {
                            self.retry(id, now);
                            None
                        } else {
                            Some(self.finish(id, Err(Failure::Refused(error))))
                        }
                    }
                }
            }
            (
                Stage::Querying {
                    operation,
                    tag: t,
                    seen,
                },
                FeatureReply::Query(result),
            ) if t == tag => {
                self.tags.remove(&tag);
                let state = match result {
                    Ok(QueryReply::Browser(BrowserReply::Operation(op)))
                        if op.operation_id == operation =>
                    {
                        op.state
                    }
                    Ok(_) => {
                        return Some(self.finish(
                            id,
                            Err(Failure::Unexpected(
                                "the operation answer is not the one asked",
                            )),
                        ));
                    }
                    Err(error) => return Some(self.finish(id, Err(Failure::Refused(error)))),
                };
                match state {
                    RendererOperationState::Running => {
                        let op = self.ops.get_mut(&id).unwrap();
                        op.stage = if seen == self.event_seq {
                            Stage::Waiting { operation }
                        } else {
                            // Something changed after this was asked for: the
                            // event may have been the end of this operation.
                            Stage::NeedQuery { operation }
                        };
                        None
                    }
                    RendererOperationState::Completed { output } => {
                        Some(self.finish(id, Ok(output)))
                    }
                    RendererOperationState::Failed { reason }
                    | RendererOperationState::Cancelled { reason } => {
                        // A failed operation carries no retry advice, so nothing
                        // is sent again for it.
                        Some(self.finish(id, Err(Failure::Renderer(reason))))
                    }
                }
            }
            // A reply of another kind or of an older step: not for this stage.
            _ => None,
        }
    }
}
