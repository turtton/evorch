//! The shared operation and management vocabulary (S0): it round-trips on the
//! wire, refuses malformed ids, and is not yet an operation of any namespace.
use serde_json::json;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{MemoryId, OperationId};
use workbench_protocol::management::*;
use workbench_protocol::namespaces::{NAMESPACES, NamespaceStatus};
use workbench_protocol::operation::*;
use workbench_protocol::wire::Revision;

fn round_trip<T>(value: &T, wire: serde_json::Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(value).unwrap(), wire);
    assert_eq!(&serde_json::from_value::<T>(wire).unwrap(), value);
}

#[test]
fn operation_states_have_a_stable_wire_form_and_only_pending_is_open() {
    round_trip(&OperationState::Pending, json!({"type": "pending"}));
    round_trip(&OperationState::Ready, json!({"type": "ready"}));
    round_trip(
        &OperationState::Failed {
            code: ErrorCode::Conflict,
        },
        json!({"type": "failed", "code": "conflict"}),
    );
    round_trip(
        &OperationState::Cancelled {
            reason: CancelReason::Restart,
        },
        json!({"type": "cancelled", "reason": "restart"}),
    );
    assert!(!OperationState::Pending.is_final());
    assert!(OperationState::Ready.is_final());
}

#[test]
fn only_a_ready_operation_names_a_result() {
    let view = |state, result: Option<&str>| OperationView {
        operation_id: OperationId::new("op-1").unwrap(),
        state,
        revision: Revision(3),
        result: result.map(|r| ResultRef::new(r).unwrap()),
    };
    assert!(
        view(OperationState::Ready, Some("report-1"))
            .validate()
            .is_ok()
    );
    assert!(view(OperationState::Ready, None).validate().is_ok());
    assert!(
        view(OperationState::Pending, Some("report-1"))
            .validate()
            .is_err()
    );
    round_trip(
        &view(OperationState::Ready, Some("report-1")),
        json!({"operation_id": "op-1", "state": {"type": "ready"}, "revision": "3", "result": "report-1"}),
    );
}

#[test]
fn ids_and_result_refs_refuse_malformed_values() {
    assert!(OperationId::new("").is_err());
    assert!(OperationId::new("a\nb").is_err());
    assert!(serde_json::from_value::<OperationId>(json!("")).is_err());
    assert!(ResultRef::new("x".repeat(MAX_RESULT_REF_BYTES + 1)).is_err());
}

#[test]
fn memory_state_keeps_favorite_apart_from_status_and_explains_denials() {
    let state = MemoryState {
        id: MemoryId::new("mem-1").unwrap(),
        status: MemoryStatus::Promoted,
        favorite: true,
        revision: Revision(7),
        actions: vec![
            ActionAvailability {
                action: MemoryAction::Reject,
                state: ActionState::Denied {
                    reason: DenialReason::FavoriteMustBeCleared,
                },
            },
            ActionAvailability {
                action: MemoryAction::ClearFavorite,
                state: ActionState::Allowed,
            },
        ],
    };
    round_trip(
        &state,
        json!({
            "id": "mem-1", "status": "promoted", "favorite": true, "revision": "7",
            "actions": [
                {"action": "reject", "state": {"type": "denied", "reason": "favorite_must_be_cleared"}},
                {"action": "clear_favorite", "state": {"type": "allowed"}}
            ]
        }),
    );
}

#[test]
fn evaluation_artifacts_tell_missing_from_a_problem() {
    let artifact =
        |presence| json!({"group": "g", "member": "m", "kind": "report", "presence": presence});
    for (presence, name) in [
        (ArtifactPresence::Present, "present"),
        (ArtifactPresence::Missing, "missing"),
        (ArtifactPresence::Problem, "problem"),
    ] {
        round_trip(
            &EvaluationArtifactRef {
                group: "g".to_owned().try_into().unwrap(),
                member: "m".to_owned().try_into().unwrap(),
                kind: EvaluationArtifactKind::Report,
                presence,
            },
            artifact(name),
        );
    }
    assert!(EvaluationJobStatus::Succeeded.is_final());
    assert!(!EvaluationJobStatus::Running.is_final());
}

#[test]
fn the_shared_vocabulary_does_not_make_a_reserved_namespace_implemented() {
    for name in ["memory", "improvement", "evaluation"] {
        let info = NAMESPACES.iter().find(|n| n.name == name).unwrap();
        assert_eq!(info.status, NamespaceStatus::Reserved, "{name}");
    }
}
