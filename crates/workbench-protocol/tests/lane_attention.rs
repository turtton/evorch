//! Wire form and bounds of the lane attention read model.

use serde_json::json;
use workbench_protocol::ids::{QuestionId, ThreadId};
use workbench_protocol::lane_attention::*;
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

fn item(kind: LaneAttentionKind) -> LaneAttentionItem {
    LaneAttentionItem {
        id: LaneAttentionItemId::new("item-1").unwrap(),
        lane: LaneId::new("lane-1").unwrap(),
        lead_thread: ThreadId::new("lead-1").unwrap(),
        management_revision: Revision(2),
        revision: Revision(9_007_199_254_740_993),
        kind,
        since: UnixMillis(1_700_000_000_000),
        summary: "needs a decision".into(),
        evidence_refs: vec!["artifact:1".into()],
        available_actions: vec![LaneAttentionAction::Inspect, LaneAttentionAction::Continue],
    }
}

fn every_kind() -> Vec<LaneAttentionKind> {
    let thread = || ThreadId::new("dest-1").unwrap();
    vec![
        LaneAttentionKind::QuestionPending {
            question: QuestionId::new("q-1").unwrap(),
            responder: LaneQuestionResponder::Lead {
                lead_thread: thread(),
            },
            routing_revision: Revision(3),
        },
        LaneAttentionKind::QuestionPending {
            question: QuestionId::new("q-2").unwrap(),
            responder: LaneQuestionResponder::User,
            routing_revision: Revision(1),
        },
        LaneAttentionKind::Blocked {
            report_seq: Sequence(4),
        },
        LaneAttentionKind::Failed {
            report_seq: Sequence(5),
            episode: Some(LaneEpisodeId::new("ep-1").unwrap()),
        },
        LaneAttentionKind::Failed {
            report_seq: Sequence(6),
            episode: None,
        },
        LaneAttentionKind::RestoreFailed {
            destination_thread: thread(),
            failure_ref: "lane_delivery:dest-1".into(),
            attempts: 3,
            suppressed: true,
        },
        LaneAttentionKind::ReadyUnclosed {
            ready_seq: Sequence(7),
            readiness: LaneReadyState::ReviewInProgress,
        },
        LaneAttentionKind::ReadyUnclosed {
            ready_seq: Sequence(8),
            readiness: LaneReadyState::NoEvidence,
        },
        LaneAttentionKind::QuestionCapacityBlocked {
            capacity: LaneQuestionCapacity::Global,
            limit: 40,
        },
        LaneAttentionKind::WakeSuppressed {
            guard_revision: Revision(11),
        },
        LaneAttentionKind::StopPending {
            destination_thread: thread(),
            pending_count: 18_446_744_073_709_551_615,
        },
        LaneAttentionKind::Stall {
            episode: LaneEpisodeId::new("ep-2").unwrap(),
            classification: LaneStallClassification::SilentDone,
            nudge: LaneNudgeState::Pending,
        },
    ]
}

#[test]
fn every_kind_round_trips_and_validates() {
    for kind in every_kind() {
        let page = LaneAttentionPage {
            items: vec![item(kind)],
            next_cursor: Some(LaneAttentionCursor::new("c".repeat(300)).unwrap()),
            snapshot: LaneAttentionSnapshot::new("a".repeat(64)).unwrap(),
        };
        page.validate().unwrap();
        let wire = serde_json::to_string(&page).unwrap();
        assert_eq!(
            serde_json::from_str::<LaneAttentionPage>(&wire).unwrap(),
            page
        );
    }
}

#[test]
fn the_wire_form_is_tagged_and_carries_u64_as_decimal_strings() {
    let wire = serde_json::to_value(item(LaneAttentionKind::StopPending {
        destination_thread: ThreadId::new("dest-1").unwrap(),
        pending_count: 3,
    }))
    .unwrap();
    assert_eq!(
        wire,
        json!({
            "id": "item-1",
            "lane": "lane-1",
            "lead_thread": "lead-1",
            "management_revision": "2",
            "revision": "9007199254740993",
            "kind": {
                "type": "stop_pending",
                "payload": {"destination_thread": "dest-1", "pending_count": "3"}
            },
            "since": "1700000000000",
            "summary": "needs a decision",
            "evidence_refs": ["artifact:1"],
            "available_actions": ["inspect", "continue"]
        })
    );
    let lead = serde_json::to_value(LaneQuestionResponder::Lead {
        lead_thread: ThreadId::new("lead-1").unwrap(),
    })
    .unwrap();
    assert_eq!(
        lead,
        json!({"type": "lead", "payload": {"lead_thread": "lead-1"}})
    );
    assert_eq!(
        serde_json::to_value(LaneQuestionResponder::User).unwrap(),
        json!({"type": "user"})
    );
}

#[test]
fn unknown_fields_and_malformed_numbers_are_refused() {
    let mut wire = serde_json::to_value(item(LaneAttentionKind::Blocked {
        report_seq: Sequence(1),
    }))
    .unwrap();
    wire["revision"] = json!(5);
    assert!(serde_json::from_value::<LaneAttentionItem>(wire.clone()).is_err());
    wire["revision"] = json!("5");
    wire["extra"] = json!(true);
    assert!(serde_json::from_value::<LaneAttentionItem>(wire).is_err());
}

#[test]
fn ids_and_cursors_are_bounded_at_parse_time() {
    assert!(LaneId::new("").is_err());
    assert!(LaneId::new("x".repeat(129)).is_err());
    assert!(LaneAttentionCursor::new("").is_err());
    assert!(LaneAttentionCursor::new("c".repeat(513)).is_err());
    assert!(LaneAttentionCursor::new("c".repeat(512)).is_ok());
    assert!(serde_json::from_value::<LaneAttentionCursor>(json!("a\nb")).is_err());
}

#[test]
fn pages_and_items_are_bounded() {
    let mut page = LaneAttentionPage {
        items: (0..MAX_LANE_ATTENTION_ITEMS)
            .map(|_| {
                item(LaneAttentionKind::Blocked {
                    report_seq: Sequence(1),
                })
            })
            .collect(),
        next_cursor: None,
        snapshot: LaneAttentionSnapshot::new("s").unwrap(),
    };
    page.validate().unwrap();
    page.items.push(page.items[0].clone());
    assert!(page.validate().is_err());

    let mut long = item(LaneAttentionKind::Blocked {
        report_seq: Sequence(1),
    });
    long.summary = "x".repeat(2048);
    assert!(long.validate().is_err());
    let mut refs = item(LaneAttentionKind::Blocked {
        report_seq: Sequence(1),
    });
    refs.evidence_refs = vec!["r".into(); MAX_LANE_ATTENTION_REFS + 1];
    assert!(refs.validate().is_err());
}
