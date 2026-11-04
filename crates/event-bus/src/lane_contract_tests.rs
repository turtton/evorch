//! Routing projection/serde compatibility contracts, not prompt wording tests.
use crate::{AnswerOrigin, UserQuestion, lane::*};
fn legacy(child: bool) -> UserQuestion {
    serde_json::from_value(serde_json::json!({"id":"q", "run_id":if child {"child"} else {"root"}, "root_run_id":"root", "root_name":"chat:Worker:t", "title":"scope", "options":[], "blocking":true, "answer":null})).unwrap()
}
#[test]
fn legacy_visibility_and_explicit_routing_are_independent_of_inheritance() {
    let root = legacy(false);
    assert!(root.is_user_visible());
    assert_eq!(root.responder, None);
    let mut child = legacy(true);
    assert!(!child.is_user_visible());
    child.responder = Some(QuestionResponder::User);
    assert!(child.is_user_visible());
    child.responder = Some(QuestionResponder::Lead {
        thread_id: "lead".into(),
    });
    assert!(!child.is_user_visible());
    assert!(child.recipient_run_ids.is_empty());
}
#[test]
fn unbound_adapter_is_fail_safe_for_every_policy() {
    for policy in [
        LanePolicy::Attached,
        LanePolicy::Summary,
        LanePolicy::Detached,
    ] {
        assert_eq!(
            LaneDeliveryBinding::default().responder(policy, Some("lead")),
            QuestionResponder::User
        );
    }
    assert!(matches!(
        LaneDeliveryBinding::Bound.responder(LanePolicy::Attached, Some("lead")),
        QuestionResponder::Lead { .. }
    ));
    for policy in [LanePolicy::Summary, LanePolicy::Detached] {
        assert_eq!(
            LaneDeliveryBinding::Bound.responder(policy, Some("lead")),
            QuestionResponder::User
        );
    }
    assert_eq!(
        LaneDeliveryBinding::Bound.responder(LanePolicy::Attached, None),
        QuestionResponder::User
    );
}
#[test]
fn legacy_host_answers_load_but_lane_answers_are_not_human() {
    let old: AnswerOrigin = serde_json::from_str(r#"{"kind":"host"}"#).unwrap();
    assert!(matches!(old, AnswerOrigin::Host { .. }));
    let origin = AnswerOrigin::LaneLead {
        run_id: "lead-root".into(),
        thread_id: "lead".into(),
    };
    let value = serde_json::to_value(&origin).unwrap();
    assert_eq!(
        serde_json::from_value::<AnswerOrigin>(value).unwrap(),
        origin
    );
}
#[test]
fn fingerprints_require_complete_immutable_identifiers() {
    assert_eq!(
        ArtifactRevision::CommitSha("A".repeat(40)).fingerprint(),
        Some(format!("commit:{}", "a".repeat(40)))
    );
    for value in ["main", "src/file.rs", "abc123", &"z".repeat(40)] {
        assert_eq!(
            ArtifactRevision::CommitSha(value.into()).fingerprint(),
            None
        );
    }
    assert!(
        ArtifactRevision::Sha256("b".repeat(64))
            .fingerprint()
            .is_some()
    );
}
