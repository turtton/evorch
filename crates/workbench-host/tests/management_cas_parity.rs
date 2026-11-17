//! What the read areas show (`revision`, `actions`) and what the storage's conditional
//! update accepts are one definition: for every state, an update that names the shown
//! revision is accepted exactly when the shown action is allowed, and is refused for the
//! shown reason when it is denied. The improvement revision digest is pinned so its
//! meaning cannot drift.
#[path = "memory_reads/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use storage::improvement::{
    ImprovementCandidate, ImprovementRecordOutcome, ImprovementSeverity, ImprovementSource,
    ImprovementStatus as StoredImprovementStatus, ImprovementWritePolicy, NewImprovementCandidate,
};
use storage::management::{CasOutcome, CasRefusal, Denial, ImprovementUpdate, MemoryOp};
use storage::memory::{Lesson, LessonScope};
use support::{World, operation, project, world};
use workbench_host::areas::{
    Areas, diagnostics::read_ops::SlugOf, improvement::ImprovementArea, memory::MemoryArea,
};
use workbench_protocol::WorkbenchError;
use workbench_protocol::ids::{ImprovementCandidateId, MemoryId};
use workbench_protocol::management::{
    ActionAvailability, ActionState, DenialReason, ImprovementAction, MemoryAction,
};
use workbench_protocol::namespaces::improvement::{
    GetImprovement, ImprovementDetail, ImprovementQuery, ImprovementReply,
};
use workbench_protocol::namespaces::memory::{GetMemory, MemoryDetail, MemoryQuery, MemoryReply};
use workbench_protocol::queries::{Query, QueryReply};

const SLUG: &str = "acme/widgets";

fn both_world() -> World {
    world(|parts| Areas {
        memory: MemoryArea::from_parts(
            parts.tasks.clone(),
            parts.projection.source("memory").unwrap(),
            parts.database.clone(),
            Arc::new(|_: &std::path::Path| SLUG.to_owned()) as SlugOf,
        ),
        improvement: ImprovementArea::from_parts(
            parts.tasks,
            parts.projection.source("improvement").unwrap(),
            parts.database,
            Arc::new(|_: &std::path::Path| SLUG.to_owned()) as SlugOf,
            true,
        ),
        ..Default::default()
    })
}

fn memory_detail(w: &World, name: &str, id: &str) -> MemoryDetail {
    w.read(|| {
        match w.query(Query::Memory(MemoryQuery::GetMemory(GetMemory {
            operation_id: operation(name),
            project: project(),
            id: MemoryId::new(id).unwrap(),
        })))? {
            QueryReply::Memory(MemoryReply::Detail(outcome)) => Ok::<_, WorkbenchError>(*outcome),
            other => panic!("{other:?}"),
        }
    })
    .data
    .unwrap()
}

fn improvement_detail(w: &World, name: &str, id: &str) -> ImprovementDetail {
    w.read(|| {
        match w.query(Query::Improvement(ImprovementQuery::GetImprovement(
            GetImprovement {
                operation_id: operation(name),
                project: project(),
                candidate: ImprovementCandidateId::new(id).unwrap(),
            },
        )))? {
            QueryReply::Improvement(ImprovementReply::Detail(outcome)) => Ok(*outcome),
            other => panic!("{other:?}"),
        }
    })
    .data
    .unwrap()
}

fn lesson(id: &str) -> Lesson {
    Lesson {
        id: id.into(),
        project: SLUG.into(),
        task_id: "task-1".into(),
        content: format!("content of {id}"),
        evidence: format!("evidence of {id}"),
        scope: LessonScope::Project,
    }
}

fn denial_reason(denial: Denial) -> DenialReason {
    match denial {
        Denial::NotApplicableInState => DenialReason::NotApplicableInState,
        Denial::FavoriteMustBeCleared => DenialReason::FavoriteMustBeCleared,
        Denial::AlreadyResolved => DenialReason::AlreadyResolved,
    }
}

/// The shown availability of `action` against what the update did with the shown
/// revision.
fn assert_agree(state: ActionState, outcome: CasOutcome, context: &str) {
    match (state, outcome) {
        (ActionState::Allowed, CasOutcome::Applied { .. } | CasOutcome::Unchanged { .. }) => {}
        (ActionState::Denied { reason }, CasOutcome::Refused(CasRefusal::Denied(denial))) => {
            assert_eq!(reason, denial_reason(denial), "{context}")
        }
        (state, outcome) => panic!("{context}: shown {state:?} but the update did {outcome:?}"),
    }
}

fn availability<A: Copy + PartialEq>(actions: &[ActionAvailability<A>], action: A) -> ActionState {
    actions.iter().find(|a| a.action == action).unwrap().state
}

#[test]
fn a_lesson_update_accepts_what_the_read_side_allows_in_every_state() {
    let w = both_world();
    let writer = w.store.handle();
    // (status steps applied, favorite) for every state a lesson can be in.
    let states: [(&str, &[&str], bool); 7] = [
        ("candidate", &[], false),
        ("candidate-fav", &[], true),
        ("validated", &["validate"], false),
        ("validated-fav", &["validate"], true),
        ("promoted", &["validate", "promote"], false),
        ("promoted-fav", &["validate", "promote"], true),
        ("rejected", &["reject"], false),
    ];
    let ops = [
        (MemoryAction::SetFavorite, MemoryOp::SetFavorite),
        (MemoryAction::ClearFavorite, MemoryOp::ClearFavorite),
        (MemoryAction::Validate, MemoryOp::Validate),
        (MemoryAction::Promote, MemoryOp::Promote),
        (MemoryAction::Reject, MemoryOp::Reject),
    ];
    for (name, steps, favorite) in states {
        for (action, op) in ops {
            let id = format!("{name}-{op:?}");
            let seed = lesson(&id);
            writer.append_lesson(&seed).unwrap();
            for step in steps {
                match *step {
                    "validate" => writer.validate_lesson(&id, &seed.evidence).unwrap(),
                    "promote" => writer.promote_lesson(&id).unwrap(),
                    "reject" => writer.reject_lesson(&id).unwrap(),
                    other => unreachable!("{other}"),
                }
            }
            if favorite {
                writer.set_lesson_favorite(&id, true).unwrap();
            }
            let shown = memory_detail(&w, &format!("read-{id}"), &id);
            let outcome = writer
                .update_memory(&id, shown.state.revision.0, op)
                .unwrap();
            assert_agree(
                availability(&shown.state.actions, action),
                outcome,
                &format!("{op:?} on {name}"),
            );
        }
    }
}

#[test]
fn a_favorite_lesson_shows_reject_denied_and_the_update_refuses_it() {
    let w = both_world();
    let writer = w.store.handle();
    writer.append_lesson(&lesson("fav")).unwrap();
    writer.set_lesson_favorite("fav", true).unwrap();
    let shown = memory_detail(&w, "fav-read", "fav");
    assert_eq!(
        availability(&shown.state.actions, MemoryAction::Reject),
        ActionState::Denied {
            reason: DenialReason::FavoriteMustBeCleared
        }
    );
    assert_eq!(
        writer
            .update_memory("fav", shown.state.revision.0, MemoryOp::Reject)
            .unwrap(),
        CasOutcome::Refused(CasRefusal::Denied(Denial::FavoriteMustBeCleared))
    );
}

#[test]
fn the_shown_lesson_revision_is_the_one_the_update_compares() {
    let w = both_world();
    let writer = w.store.handle();
    writer.append_lesson(&lesson("rev")).unwrap();
    let first = memory_detail(&w, "rev-1", "rev").state.revision;
    let after = match writer
        .update_memory("rev", first.0, MemoryOp::SetFavorite)
        .unwrap()
    {
        CasOutcome::Applied { revision } => revision,
        other => panic!("{other:?}"),
    };
    // The revision the update reports is the one the next read shows.
    assert_eq!(memory_detail(&w, "rev-2", "rev").state.revision.0, after);
    // And the old one is stale.
    assert!(matches!(
        writer
            .update_memory("rev", first.0, MemoryOp::ClearFavorite)
            .unwrap(),
        CasOutcome::Refused(CasRefusal::StaleRevision { current }) if current == after
    ));
}

fn record(w: &World, id: &str) {
    let outcome = w
        .store
        .handle()
        .record_improvement_candidate(
            SLUG,
            NewImprovementCandidate {
                id: id.into(),
                source: ImprovementSource::Diagnostic,
                code: "tool_failure".into(),
                severity: ImprovementSeverity::Warning,
                title: format!("title of {id}"),
                evidence: format!("evidence of {id}"),
                dedup_key: format!("key-{id}"),
                run_id: None,
            },
            ImprovementWritePolicy {
                duplicate_cooldown: Duration::from_secs(60),
                daily_limit: 1000,
                run_error_daily_limit: 1000,
                max_candidates: 1000,
            },
        )
        .unwrap();
    assert!(matches!(outcome, ImprovementRecordOutcome::Stored { .. }));
}

#[test]
fn an_improvement_update_accepts_what_the_read_side_allows_in_every_state() {
    let w = both_world();
    let writer = w.store.handle();
    let ops = [
        (
            ImprovementAction::MarkReviewed,
            ImprovementUpdate::MarkReviewed,
        ),
        (ImprovementAction::Dismiss, ImprovementUpdate::Dismiss),
        (
            ImprovementAction::Triage,
            ImprovementUpdate::AttachDraft {
                draft_path: "draft.md".into(),
            },
        ),
    ];
    for status in [
        StoredImprovementStatus::New,
        StoredImprovementStatus::Reviewed,
        StoredImprovementStatus::Dismissed,
    ] {
        for (action, update) in &ops {
            let id = format!("{}-{:?}", status.as_str(), action);
            record(&w, &id);
            if status != StoredImprovementStatus::New {
                assert!(writer.set_improvement_status(&id, status).unwrap());
            }
            let shown = improvement_detail(&w, &format!("read-{id}"), &id);
            let state = &shown.summary.state;
            let outcome = writer
                .update_improvement(&id, state.revision.0, update.clone())
                .unwrap();
            assert_agree(
                availability(&state.actions, *action),
                outcome,
                &format!("{action:?} on {}", status.as_str()),
            );
        }
    }
}

#[test]
fn the_improvement_revision_keeps_its_meaning() {
    let mut candidate = ImprovementCandidate {
        id: "pinned".into(),
        project: SLUG.into(),
        created_at_ns: 1,
        source: ImprovementSource::Diagnostic,
        code: "tool_failure".into(),
        severity: ImprovementSeverity::Warning,
        title: "title".into(),
        evidence: "evidence".into(),
        dedup_key: "key".into(),
        status: StoredImprovementStatus::New,
        draft_path: None,
        run_id: None,
        occurrences: 1,
        last_seen_at_ns: 100,
        recent_run_ids: Vec::new(),
        reopened_count: 0,
        reopened_at_ns: None,
    };
    // The first eight bytes of SHA-256 over the JSON of
    // (status, occurrences, last_seen_at_ns, reopened_count, reopened_at_ns, draft_path),
    // exactly what the read side computed before the storage took the definition over.
    // `printf '%s' '["new",1,100,0,null,null]' | sha256sum`
    assert_eq!(
        workbench_host::areas::improvement::revision_of(&candidate).0,
        0xac67_e87d_afac_6024
    );
    candidate.status = StoredImprovementStatus::Dismissed;
    candidate.occurrences = 3;
    candidate.last_seen_at_ns = 900;
    candidate.reopened_count = 2;
    candidate.reopened_at_ns = Some(500);
    candidate.draft_path = Some("cand.md".into());
    // `printf '%s' '["dismissed",3,900,2,500,"cand.md"]' | sha256sum`
    assert_eq!(
        storage::management::improvement_revision(&candidate),
        0x716c_2f8a_38c5_4da0
    );
    // Fields outside the tuple do not move it.
    candidate.title = "another title".into();
    candidate.evidence = "other".into();
    assert_eq!(
        storage::management::improvement_revision(&candidate),
        0x716c_2f8a_38c5_4da0
    );
}
