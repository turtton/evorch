//! The boundary adapter between the lane ledger's domain values (`event_bus::lane`) and the
//! wire read model (`workbench_protocol::lane_attention`). Every field is mapped by name; a
//! value the wire cannot hold is an error, never silently dropped.
//!
//! The adapter sits in the client crate for now: core crates must not depend on the wire
//! crate, and the core-side host crate that will own it is not in `main` yet. It is a pure
//! function so it can move there unchanged.

use event_bus::lane as domain;
use workbench_protocol::ids::{QuestionId, ThreadId};
use workbench_protocol::lane_attention as wire;
use workbench_protocol::validation::ValidationError;
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

/// Why a domain value could not be put on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ValidationError> for WireError {
    fn from(error: ValidationError) -> Self {
        Self(error.to_string())
    }
}

const NANOS_PER_MILLI: i64 = 1_000_000;

/// Converts one page of the ledger's attention list.
///
/// # Errors
/// An id, cursor or text the wire refuses, or a kind this adapter does not know.
pub fn page(page: &domain::LaneAttentionPage) -> Result<wire::LaneAttentionPage, WireError> {
    let converted = wire::LaneAttentionPage {
        items: page.items.iter().map(item).collect::<Result<_, _>>()?,
        next_cursor: page
            .next_cursor
            .as_ref()
            .map(wire::LaneAttentionCursor::new)
            .transpose()?,
        snapshot: wire::LaneAttentionSnapshot::new(page.snapshot_revision.clone())?,
    };
    converted.validate()?;
    Ok(converted)
}

fn item(item: &domain::LaneAttentionItem) -> Result<wire::LaneAttentionItem, WireError> {
    Ok(wire::LaneAttentionItem {
        id: wire::LaneAttentionItemId::new(item.id.clone())?,
        lane: wire::LaneId::new(item.lane_id.clone())?,
        lead_thread: ThreadId::new(item.lead_thread_id.clone())?,
        management_revision: Revision(item.management_revision),
        revision: Revision(item.revision),
        kind: kind(&item.kind)?,
        since: UnixMillis(u64::try_from(item.since_at_ns.max(0) / NANOS_PER_MILLI).unwrap_or(0)),
        summary: item.summary.clone(),
        evidence_refs: item.evidence_refs.clone(),
        available_actions: item.available_actions.iter().copied().map(action).collect(),
    })
}

fn kind(kind: &domain::LaneAttentionKind) -> Result<wire::LaneAttentionKind, WireError> {
    use domain::LaneAttentionKind as D;
    use wire::LaneAttentionKind as W;
    Ok(match kind {
        D::QuestionPending {
            question_id,
            responder,
            routing_revision,
        } => W::QuestionPending {
            question: QuestionId::new(question_id.clone())?,
            responder: match responder {
                domain::QuestionResponder::User => wire::LaneQuestionResponder::User,
                domain::QuestionResponder::Lead { thread_id } => {
                    wire::LaneQuestionResponder::Lead {
                        lead_thread: ThreadId::new(thread_id.clone())?,
                    }
                }
                _ => return Err(WireError("unknown question responder".into())),
            },
            routing_revision: Revision(*routing_revision),
        },
        D::Blocked { report_seq } => W::Blocked {
            report_seq: Sequence(*report_seq),
        },
        D::Failed {
            report_seq,
            episode_id,
        } => W::Failed {
            report_seq: Sequence(*report_seq),
            episode: episode_id
                .clone()
                .map(wire::LaneEpisodeId::new)
                .transpose()?,
        },
        D::RestoreFailed {
            destination_thread_id,
            failure_ref,
            attempts,
            suppressed,
        } => W::RestoreFailed {
            destination_thread: ThreadId::new(destination_thread_id.clone())?,
            failure_ref: failure_ref.clone(),
            attempts: *attempts,
            suppressed: *suppressed,
        },
        D::ReadyUnclosed {
            ready_seq,
            readiness,
        } => W::ReadyUnclosed {
            ready_seq: Sequence(*ready_seq),
            readiness: ready_state_of(*readiness),
        },
        D::QuestionCapacityBlocked { capacity, limit } => W::QuestionCapacityBlocked {
            capacity: match capacity {
                domain::LaneQuestionCapacity::Requester => wire::LaneQuestionCapacity::Requester,
            },
            limit: *limit,
        },
        D::WakeSuppressed { guard_revision } => W::WakeSuppressed {
            guard_revision: Revision(*guard_revision),
        },
        D::StopPending {
            destination_thread_id,
            pending_count,
        } => W::StopPending {
            destination_thread: ThreadId::new(destination_thread_id.clone())?,
            pending_count: *pending_count,
        },
        D::Stall {
            episode_id,
            classification,
            nudge_state,
        } => W::Stall {
            episode: wire::LaneEpisodeId::new(episode_id.clone())?,
            classification: match classification {
                domain::LaneStallClassification::NoReport => {
                    wire::LaneStallClassification::NoReport
                }
                domain::LaneStallClassification::WorkStalled => {
                    wire::LaneStallClassification::WorkStalled
                }
                domain::LaneStallClassification::WaitingQuestion => {
                    wire::LaneStallClassification::WaitingQuestion
                }
                domain::LaneStallClassification::SilentDone => {
                    wire::LaneStallClassification::SilentDone
                }
                domain::LaneStallClassification::RestoreUnavailable => {
                    wire::LaneStallClassification::RestoreUnavailable
                }
            },
            nudge: match nudge_state {
                domain::LaneNudgeState::NotRequested => wire::LaneNudgeState::NotRequested,
                domain::LaneNudgeState::Pending => wire::LaneNudgeState::Pending,
                domain::LaneNudgeState::Accepted => wire::LaneNudgeState::Accepted,
                domain::LaneNudgeState::Suppressed => wire::LaneNudgeState::Suppressed,
                domain::LaneNudgeState::Obsolete => wire::LaneNudgeState::Obsolete,
                domain::LaneNudgeState::Failed => wire::LaneNudgeState::Failed,
            },
        },
    })
}

const fn ready_state_of(state: domain::LaneReadyState) -> wire::LaneReadyState {
    match state {
        domain::LaneReadyState::NoEvidence => wire::LaneReadyState::NoEvidence,
        domain::LaneReadyState::Unreviewed => wire::LaneReadyState::Unreviewed,
        domain::LaneReadyState::ReviewInProgress => wire::LaneReadyState::ReviewInProgress,
        domain::LaneReadyState::ReviewRejected => wire::LaneReadyState::ReviewRejected,
        domain::LaneReadyState::ReviewIncomplete => wire::LaneReadyState::ReviewIncomplete,
        domain::LaneReadyState::AwaitingClose => wire::LaneReadyState::AwaitingClose,
    }
}

const fn action(action: domain::LaneAttentionAction) -> wire::LaneAttentionAction {
    match action {
        domain::LaneAttentionAction::Answer => wire::LaneAttentionAction::Answer,
        domain::LaneAttentionAction::RouteToUser => wire::LaneAttentionAction::RouteToUser,
        domain::LaneAttentionAction::Continue => wire::LaneAttentionAction::Continue,
        domain::LaneAttentionAction::Inspect => wire::LaneAttentionAction::Inspect,
        domain::LaneAttentionAction::Verify => wire::LaneAttentionAction::Verify,
        domain::LaneAttentionAction::Close => wire::LaneAttentionAction::Close,
        domain::LaneAttentionAction::Resume => wire::LaneAttentionAction::Resume,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain_item(kind: domain::LaneAttentionKind) -> domain::LaneAttentionItem {
        domain::LaneAttentionItem {
            id: "item".into(),
            lane_id: "lane".into(),
            lead_thread_id: "lead".into(),
            management_revision: 2,
            revision: 5,
            kind,
            since_at_ns: 3_000_000_000,
            summary: "summary".into(),
            evidence_refs: vec!["ref".into()],
            available_actions: vec![domain::LaneAttentionAction::Inspect],
        }
    }

    fn domain_page(kinds: Vec<domain::LaneAttentionKind>) -> domain::LaneAttentionPage {
        domain::LaneAttentionPage {
            items: kinds.into_iter().map(domain_item).collect(),
            next_cursor: Some("cursor".into()),
            snapshot_revision: "a".repeat(64),
        }
    }

    #[test]
    fn every_kind_is_mapped_field_by_field() {
        use domain::LaneAttentionKind as D;
        use wire::LaneAttentionKind as W;
        let converted = page(&domain_page(vec![
            D::QuestionPending {
                question_id: "q".into(),
                responder: domain::QuestionResponder::Lead {
                    thread_id: "lead".into(),
                },
                routing_revision: 4,
            },
            D::Blocked { report_seq: 6 },
            D::Failed {
                report_seq: 7,
                episode_id: Some("ep".into()),
            },
            D::RestoreFailed {
                destination_thread_id: "dest".into(),
                failure_ref: "lane_delivery:dest".into(),
                attempts: 3,
                suppressed: true,
            },
            D::ReadyUnclosed {
                ready_seq: 8,
                readiness: domain::LaneReadyState::AwaitingClose,
            },
            D::QuestionCapacityBlocked {
                capacity: domain::LaneQuestionCapacity::Requester,
                limit: 40,
            },
            D::WakeSuppressed { guard_revision: 9 },
            D::StopPending {
                destination_thread_id: "dest".into(),
                pending_count: 3,
            },
            D::Stall {
                episode_id: "ep2".into(),
                classification: domain::LaneStallClassification::SilentDone,
                nudge_state: domain::LaneNudgeState::Accepted,
            },
        ]))
        .unwrap();

        let kinds: Vec<_> = converted.items.iter().map(|item| &item.kind).collect();
        assert!(matches!(
            kinds[0],
            W::QuestionPending {
                responder: wire::LaneQuestionResponder::Lead { .. },
                routing_revision: Revision(4),
                ..
            }
        ));
        assert!(matches!(
            kinds[1],
            W::Blocked {
                report_seq: Sequence(6)
            }
        ));
        assert!(matches!(
            kinds[2],
            W::Failed {
                report_seq: Sequence(7),
                episode: Some(_)
            }
        ));
        assert!(matches!(
            kinds[3],
            W::RestoreFailed {
                attempts: 3,
                suppressed: true,
                ..
            }
        ));
        assert!(matches!(
            kinds[4],
            W::ReadyUnclosed {
                readiness: wire::LaneReadyState::AwaitingClose,
                ..
            }
        ));
        assert!(matches!(
            kinds[5],
            W::QuestionCapacityBlocked {
                capacity: wire::LaneQuestionCapacity::Requester,
                limit: 40
            }
        ));
        assert!(matches!(
            kinds[6],
            W::WakeSuppressed {
                guard_revision: Revision(9)
            }
        ));
        assert!(matches!(
            kinds[7],
            W::StopPending {
                pending_count: 3,
                ..
            }
        ));
        assert!(matches!(
            kinds[8],
            W::Stall {
                classification: wire::LaneStallClassification::SilentDone,
                nudge: wire::LaneNudgeState::Accepted,
                ..
            }
        ));

        let first = &converted.items[0];
        assert_eq!(first.since, UnixMillis(3_000));
        assert_eq!(first.management_revision, Revision(2));
        assert_eq!(
            first.available_actions,
            [wire::LaneAttentionAction::Inspect]
        );
        assert_eq!(converted.next_cursor.unwrap().as_str(), "cursor");
        assert_eq!(converted.snapshot.as_str(), "a".repeat(64));
    }

    #[test]
    fn a_value_the_wire_refuses_is_an_error_not_a_dropped_item() {
        let mut bad = domain_page(vec![domain::LaneAttentionKind::Blocked { report_seq: 1 }]);
        bad.items[0].lead_thread_id = String::new();
        assert!(page(&bad).is_err());

        let mut long = domain_page(vec![domain::LaneAttentionKind::Blocked { report_seq: 1 }]);
        long.items[0].summary = "x".repeat(4096);
        assert!(page(&long).is_err());
    }
}
