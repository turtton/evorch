//! The `improvement` reads (PR 13) over a real database: the candidates of a
//! project, the settings switch, the draft shown by file name only, and that
//! asking changes nothing.
#[path = "memory_reads/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use storage::improvement::{
    ImprovementSeverity, ImprovementSource, ImprovementStatus, ImprovementWritePolicy,
    NewImprovementCandidate,
};
use support::{World, operation, project, ready, world};
use workbench_host::areas::{Areas, diagnostics::read_ops::SlugOf, improvement::ImprovementArea};
use workbench_protocol::WorkbenchError;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::ImprovementCandidateId;
use workbench_protocol::management::{ActionState, DenialReason, ImprovementAction};
use workbench_protocol::namespaces::diagnostics::ReadOutcome;
use workbench_protocol::namespaces::improvement::{
    GetImprovement, ImprovementDetail, ImprovementList, ImprovementQuery, ImprovementReply,
    ListImprovement,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{PageRequest, Query, QueryReply};

const SLUG: &str = "acme/widgets";

fn improvement_world(enabled: bool) -> World {
    world(|parts| Areas {
        improvement: ImprovementArea::from_parts(
            parts.tasks,
            parts.projection.source("improvement").unwrap(),
            parts.database,
            Arc::new(|_: &std::path::Path| SLUG.to_owned()) as SlugOf,
            enabled,
        ),
        ..Default::default()
    })
}

fn policy() -> ImprovementWritePolicy {
    ImprovementWritePolicy {
        duplicate_cooldown: Duration::from_secs(60),
        daily_limit: 100,
        run_error_daily_limit: 100,
        max_candidates: 100,
    }
}

fn record(w: &World, project: &str, id: &str, title: &str) {
    let outcome = w
        .store
        .handle()
        .record_improvement_candidate(
            project,
            NewImprovementCandidate {
                id: id.into(),
                source: ImprovementSource::Diagnostic,
                code: "tool_failure".into(),
                severity: ImprovementSeverity::Warning,
                title: title.into(),
                evidence: format!("evidence of {id}"),
                dedup_key: format!("key-{id}"),
                run_id: None,
            },
            policy(),
        )
        .unwrap();
    assert!(
        matches!(
            outcome,
            storage::improvement::ImprovementRecordOutcome::Stored { .. }
        ),
        "{outcome:?}"
    );
}

fn list_request(id: &str) -> ListImprovement {
    ListImprovement {
        operation_id: operation(id),
        project: project(),
        status: None,
        page: PageRequest::default(),
    }
}

fn list(
    w: &World,
    request: ListImprovement,
) -> Result<ReadOutcome<ImprovementList>, WorkbenchError> {
    match w.query(Query::Improvement(ImprovementQuery::ListImprovement(
        request,
    )))? {
        QueryReply::Improvement(ImprovementReply::List(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn detail(
    w: &World,
    id: &str,
    candidate: &str,
) -> Result<ReadOutcome<ImprovementDetail>, WorkbenchError> {
    match w.query(Query::Improvement(ImprovementQuery::GetImprovement(
        GetImprovement {
            operation_id: operation(id),
            project: project(),
            candidate: ImprovementCandidateId::new(candidate).unwrap(),
        },
    )))? {
        QueryReply::Improvement(ImprovementReply::Detail(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_list_holds_this_projects_candidates_and_reports_the_switch() {
    for enabled in [true, false] {
        let w = improvement_world(enabled);
        record(&w, SLUG, "cand-a", "A tool failed");
        record(&w, SLUG, "cand-b", "Another tool failed");
        record(&w, "someone/else", "cand-other", "Not this project");
        assert!(list(&w, list_request("l")).unwrap().data.is_none());
        w.settle();
        let outcome = list(&w, list_request("l")).unwrap();
        assert!(ready(&outcome.operation.state));
        let data = outcome.data.unwrap();
        assert_eq!(data.enabled, enabled);
        let mut ids: Vec<_> = data
            .candidates
            .items
            .iter()
            .map(|c| c.state.id.as_str().to_owned())
            .collect();
        ids.sort();
        assert_eq!(ids, ["cand-a", "cand-b"]);
    }
}

#[test]
fn a_status_filter_and_the_actions_follow_the_stored_status() {
    let w = improvement_world(true);
    record(&w, SLUG, "cand-a", "A tool failed");
    record(&w, SLUG, "cand-b", "Another tool failed");
    assert!(
        w.store
            .handle()
            .set_improvement_status("cand-a", ImprovementStatus::Dismissed)
            .unwrap()
    );
    let dismissed = w.read(|| {
        list(
            &w,
            ListImprovement {
                status: Some(workbench_protocol::management::ImprovementStatus::Dismissed),
                ..list_request("dismissed")
            },
        )
    });
    let items = dismissed.data.unwrap().candidates.items;
    assert_eq!(items.len(), 1);
    let state = &items[0].state;
    assert_eq!(state.id.as_str(), "cand-a");
    let action = |a: ImprovementAction| state.actions.iter().find(|x| x.action == a).unwrap().state;
    assert_eq!(
        action(ImprovementAction::Dismiss),
        ActionState::Denied {
            reason: DenialReason::AlreadyResolved
        }
    );
    assert_eq!(
        action(ImprovementAction::MarkReviewed),
        ActionState::Allowed
    );

    let open = w.read(|| list(&w, list_request("all")));
    let revisions: Vec<_> = open
        .data
        .unwrap()
        .candidates
        .items
        .iter()
        .map(|c| c.state.revision)
        .collect();
    assert_eq!(revisions.len(), 2);
    assert_ne!(revisions[0], revisions[1]);
}

#[test]
fn a_detail_names_the_draft_by_file_name_only() {
    let w = improvement_world(true);
    record(&w, SLUG, "cand-a", "A tool failed");
    assert!(
        w.store
            .handle()
            .attach_improvement_draft("cand-a", "/home/someone/private/drafts/cand-a.md")
            .unwrap()
    );
    let found = w.read(|| detail(&w, "detail", "cand-a"));
    let data = found.data.unwrap();
    assert_eq!(data.draft_file.as_deref(), Some("cand-a.md"));
    let wire = serde_json::to_string(&data).unwrap();
    assert!(!wire.contains("/home/someone"), "{wire}");
    assert_eq!(data.evidence.text, "evidence of cand-a");
}

#[test]
fn a_foreign_or_unknown_candidate_is_not_found() {
    let w = improvement_world(true);
    record(&w, "someone/else", "cand-other", "Not this project");
    for (n, id) in ["cand-other", "cand-missing"].into_iter().enumerate() {
        let found = w.read(|| detail(&w, &format!("nf-{n}"), id));
        assert!(found.data.is_none());
        assert_eq!(
            found.operation.state,
            OperationState::Failed {
                code: ErrorCode::NotFound
            }
        );
    }
}

#[test]
fn asking_changes_nothing_and_an_unwired_area_is_unsupported() {
    let w = improvement_world(true);
    record(&w, SLUG, "cand-a", "A tool failed");
    let revision_of = |w: &World, id: &str| {
        let db = storage::Database::open_read_only(&w.dir.path().join("host.db")).unwrap();
        let candidate = db.improvement_candidate(id).unwrap().unwrap();
        (
            candidate.status,
            candidate.occurrences,
            candidate.last_seen_at_ns,
        )
    };
    let before = revision_of(&w, "cand-a");
    w.read(|| list(&w, list_request("a")));
    w.read(|| detail(&w, "b", "cand-a"));
    assert_eq!(revision_of(&w, "cand-a"), before);

    let unwired = world(|_| Areas::default());
    let refused = list(&unwired, list_request("x"));
    assert_eq!(refused.unwrap_err().code, ErrorCode::UnsupportedCapability);
}
