//! The improvement feature drawn headless: the list, the settings switch, a
//! candidate's detail with its draft named by file only, and the dropping of an
//! answer for another project.
use super::*;
use crate::core_client::features::{
    FeatureError, FeatureRequest,
    execution::tests::{answer, asked, command_of, query_of, screen},
};
use egui_kittest::kittest::{NodeT, Queryable};
use workbench_protocol::{
    ids::{OperationId, RunId},
    management::{DenialReason, ImprovementState},
    namespaces::{
        diagnostics::{BoundedText, ReadOutcome},
        improvement::{ImprovementList, ImprovementSeverity, ImprovementSource},
    },
    operation::{OperationState, OperationView},
    views::Page,
    wire::Revision,
};

fn view(id: &OperationId, state: OperationState) -> OperationView {
    OperationView {
        operation_id: id.clone(),
        state,
        revision: Revision(1),
        result: None,
    }
}

fn summary(id: &str, status: ImprovementStatus) -> ImprovementSummary {
    ImprovementSummary {
        state: ImprovementState {
            id: ImprovementCandidateId::new(id).unwrap(),
            status,
            revision: Revision(1),
            actions: vec![],
        },
        source: ImprovementSource::Diagnostic,
        severity: ImprovementSeverity::Warning,
        code: "tool_failure".into(),
        title: format!("title of {id}"),
        occurrences: 3,
        last_seen_ms: 1,
    }
}

fn list_reply(id: &OperationId, enabled: bool, items: Vec<ImprovementSummary>) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Improvement(ImprovementReply::List(
        Box::new(ReadOutcome {
            operation: view(id, OperationState::Ready),
            data: Some(ImprovementList {
                enabled,
                candidates: Page {
                    items,
                    next_cursor: None,
                    snapshot_revision: Revision(1),
                },
            }),
        }),
    ))))
}

fn list_of(action: &crate::core_client::features::FeatureAction) -> ListImprovement {
    match query_of(action) {
        Query::Improvement(ImprovementQuery::ListImprovement(list)) => list.clone(),
        other => panic!("a list was expected, got {other:?}"),
    }
}

#[test]
fn candidates_the_switch_and_a_detail_follow_what_the_core_answered() {
    let mut screen = screen(Box::<ImprovementFeature>::default(), None);
    let first = asked(&mut screen);
    let list = list_of(&first[0]);
    answer(
        &mut screen,
        first[0].tag,
        list_reply(
            &list.operation_id,
            false,
            vec![summary("cand-a", ImprovementStatus::New)],
        ),
    );
    screen.get_by_label_contains(crate::panes::improvement_widgets::DISABLED);
    screen.get_by_label("cand-a");
    screen.get_by_label("new");
    screen.get_by_label("warning");
    screen.get_by_label("title of cand-a");
    screen.get_by_label_contains("Seen 3 times");
    screen.get_by_label("1 candidates");

    screen.get_by_label("cand-a").click();
    let detail = asked(&mut screen);
    let Query::Improvement(ImprovementQuery::GetImprovement(get)) = query_of(&detail[0]) else {
        panic!("a detail was expected");
    };
    assert_eq!(get.candidate.as_str(), "cand-a");
    answer(
        &mut screen,
        detail[0].tag,
        FeatureReply::Query(Ok(QueryReply::Improvement(ImprovementReply::Detail(
            Box::new(ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(ImprovementDetail {
                    summary: summary("cand-a", ImprovementStatus::New),
                    evidence: BoundedText {
                        text: "the tool exited 2".into(),
                        total_bytes: 17,
                        truncated: false,
                    },
                    draft_file: Some("cand-a.md".into()),
                    run: Some(RunId::new("run-first").unwrap()),
                    recent_runs: vec![
                        RunId::new("run-first").unwrap(),
                        RunId::new("run-last").unwrap(),
                    ],
                    created_ms: 1,
                    reopened_count: 2,
                }),
            }),
        )))),
    );
    screen.get_by_label("Draft: cand-a.md");
    screen.get_by_label("Reopened ×2");
    assert_eq!(screen.query_all_by_label("title of cand-a").count(), 2);
    // The evidence is folded, as in the legacy pane, with the runs behind it.
    assert!(screen.query_by_label("the tool exited 2").is_none());
    screen.get_by_label("Evidence").click();
    screen.run();
    screen.get_by_label("the tool exited 2");
    screen.get_by_label("Copy run ID");
    screen.get_by_label("Recent runs: run-first, run-last");
}

#[test]
fn an_answer_for_a_project_no_longer_shown_is_dropped_and_only_queries_are_sent() {
    let mut screen = screen(Box::<ImprovementFeature>::default(), None);
    let first = asked(&mut screen);
    let old = list_of(&first[0]);
    screen.state_mut().selection.project = Some(ProjectId::new("q").unwrap());
    let second = asked(&mut screen);
    assert_eq!(list_of(&second[0]).project.as_str(), "q");
    answer(
        &mut screen,
        first[0].tag,
        list_reply(
            &old.operation_id,
            true,
            vec![summary("stale", ImprovementStatus::New)],
        ),
    );
    assert!(screen.query_by_label_contains("stale").is_none());
}

fn screen_with_detail()
-> egui_kittest::Harness<'static, crate::core_client::features::execution::tests::Screen> {
    let mut screen = screen(Box::<ImprovementFeature>::default(), None);
    let first = asked(&mut screen);
    let list = list_of(&first[0]);
    answer(
        &mut screen,
        first[0].tag,
        list_reply(
            &list.operation_id,
            true,
            vec![summary("cand-a", ImprovementStatus::New)],
        ),
    );
    screen.get_by_label("cand-a").click();
    let detail = asked(&mut screen);
    let Query::Improvement(ImprovementQuery::GetImprovement(get)) = query_of(&detail[0]) else {
        panic!("a detail was expected");
    };
    let mut item = summary("cand-a", ImprovementStatus::New);
    item.state.revision = Revision(5);
    item.state.actions = vec![
        ActionAvailability {
            action: ImprovementAction::Triage,
            state: ActionState::Allowed,
        },
        ActionAvailability {
            action: ImprovementAction::Dismiss,
            state: ActionState::Denied {
                reason: DenialReason::AlreadyResolved,
            },
        },
    ];
    answer(
        &mut screen,
        detail[0].tag,
        FeatureReply::Query(Ok(QueryReply::Improvement(ImprovementReply::Detail(
            Box::new(ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(ImprovementDetail {
                    summary: item,
                    evidence: BoundedText {
                        text: "evidence".into(),
                        total_bytes: 8,
                        truncated: false,
                    },
                    draft_file: None,
                    run: None,
                    recent_runs: vec![],
                    created_ms: 1,
                    reopened_count: 0,
                }),
            }),
        )))),
    );
    screen
}

#[test]
fn a_triage_names_the_candidate_and_its_revision_only_and_shows_the_draft_by_file() {
    let mut screen = screen_with_detail();
    assert!(
        screen
            .get_by_label("Dismiss")
            .accesskit_node()
            .is_disabled()
    );
    screen.get_by_label("Create draft").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let Command::Improvement(ImprovementCommand::TriageImprovement(triage)) = command_of(&sent[0])
    else {
        panic!("a triage was expected");
    };
    assert_eq!(triage.subject.candidate.as_str(), "cand-a");
    assert_eq!(triage.subject.expected_revision, Revision(5));
    let id = triage.operation_id.clone();

    screen.state_mut().feature.event(&UiEvent::Improvement(
        ImprovementEvent::ImprovementUpdateCompleted {
            update: ImprovementUpdateView {
                operation: view(&id, OperationState::Ready),
                project: ProjectId::new("p").unwrap(),
                candidate: ImprovementCandidateId::new("cand-a").unwrap(),
                action: ImprovementAction::Triage,
                revision: Some(Revision(6)),
                changed: Some(true),
                draft_file: Some("cand-a-draft.md".into()),
                denial: None,
                message: None,
            },
        },
    ));
    let again = asked(&mut screen);
    screen.get_by_label_contains("Draft: cand-a-draft.md (nothing is sent).");
    assert!(
        again
            .iter()
            .all(|action| matches!(action.request, FeatureRequest::Query(_))),
        "the candidate is read again; nothing is resent"
    );
}

#[test]
fn an_update_of_unknown_outcome_is_asked_for_and_never_counted_as_done() {
    let mut screen = screen_with_detail();
    screen.get_by_label("Create draft").click();
    let sent = asked(&mut screen);
    let Command::Improvement(ImprovementCommand::TriageImprovement(triage)) = command_of(&sent[0])
    else {
        panic!("a triage was expected");
    };
    let id = triage.operation_id.clone();
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    let check = asked(&mut screen);
    assert_eq!(check.len(), 1);
    let Query::Improvement(ImprovementQuery::GetImprovementUpdate(target)) = query_of(&check[0])
    else {
        panic!("the update was to be asked for");
    };
    assert_eq!(target.operation_id, id);
    answer(
        &mut screen,
        check[0].tag,
        FeatureReply::Query(Err(
            FeatureError::new("no such operation").with_code(ErrorCode::NotFound)
        )),
    );
    screen.get_by_label_contains("not counted as done");
    assert!(screen.query_by_label_contains("Draft:").is_none());
}

#[test]
fn the_candidate_status_filter_asks_for_that_status_only() {
    let mut screen = screen(Box::<ImprovementFeature>::default(), None);
    let first = list_of(&asked(&mut screen)[0]);
    assert_eq!(first.status, None);
    screen.get_by_label("Candidate status").click();
    screen.run();
    screen.get_by_label("dismissed").click();
    let next = asked(&mut screen);
    assert_eq!(next.len(), 1);
    let next = list_of(&next[0]);
    assert_eq!(next.status, Some(ImprovementStatus::Dismissed));
    assert_ne!(next.operation_id, first.operation_id);
}

#[test]
fn no_refresh_button_is_drawn_and_showing_the_candidates_again_reads_them_once() {
    let mut screen = screen_with_detail();
    assert!(screen.query_by_label("Refresh").is_none());
    for _ in 0..3 {
        assert!(
            asked(&mut screen).is_empty(),
            "no read while they stay shown"
        );
    }
    screen.state_mut().visible = false;
    assert!(
        asked(&mut screen).is_empty(),
        "no read while they are hidden"
    );
    screen.state_mut().visible = true;
    let asks = asked(&mut screen);
    assert_eq!(asks.len(), 2, "the list and the open candidate, once each");
    assert!(matches!(
        query_of(&asks[0]),
        Query::Improvement(ImprovementQuery::ListImprovement(_))
    ));
    assert!(matches!(
        query_of(&asks[1]),
        Query::Improvement(ImprovementQuery::GetImprovement(_))
    ));
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn a_failed_list_and_a_failed_detail_each_offer_their_own_retry() {
    let mut screen = screen(Box::<ImprovementFeature>::default(), None);
    let first = asked(&mut screen);
    let list = list_of(&first[0]);
    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Err(FeatureError::new("the core went away"))),
    );
    screen.get_by_label("Retry list").click();
    let retry = asked(&mut screen);
    assert_eq!(retry.len(), 1, "one click, one read");
    let retried = list_of(&retry[0]);
    assert_ne!(retried.operation_id, list.operation_id);
    answer(
        &mut screen,
        retry[0].tag,
        list_reply(
            &retried.operation_id,
            true,
            vec![summary("cand-a", ImprovementStatus::New)],
        ),
    );
    assert!(screen.query_by_label("Retry list").is_none());

    screen.get_by_label("cand-a").click();
    let detail = asked(&mut screen);
    answer(
        &mut screen,
        detail[0].tag,
        FeatureReply::Query(Err(FeatureError::new("no detail"))),
    );
    assert!(screen.query_by_label("Retry list").is_none());
    screen.get_by_label("Retry detail").click();
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert!(matches!(
        query_of(&again[0]),
        Query::Improvement(ImprovementQuery::GetImprovement(_))
    ));
}

#[test]
fn a_time_past_what_chrono_holds_reads_as_unknown_and_an_ordinary_one_as_legacy() {
    use std::time::{Duration, UNIX_EPOCH};
    let now = UNIX_EPOCH + Duration::from_secs(951_827_696 + 3_742);
    assert_eq!(
        when_ms(951_827_696_000, now, "Unknown creation time"),
        ("1h 2m ago".into(), "2000-02-29 12:34 UTC".into())
    );
    for ms in [8_300_000_000_000_000, i64::MAX as u64, u64::MAX] {
        assert_eq!(
            when_ms(ms, now, "Unknown creation time"),
            ("unknown time".into(), "Unknown creation time".into())
        );
    }
}
