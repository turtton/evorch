//! The improvement feature drawn headless: the list, the settings switch, a
//! candidate's detail with its draft named by file only, and the dropping of an
//! answer for another project.
use super::*;
use crate::core_client::features::{
    FeatureError, FeatureRequest,
    execution::tests::{answer, asked, command_of, query_of, screen},
};
use egui_kittest::kittest::Queryable;
use workbench_protocol::{
    ids::OperationId,
    management::{DenialReason, ImprovementState},
    namespaces::{
        diagnostics::{BoundedText, ReadOutcome},
        improvement::ImprovementList,
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
    screen.get_by_label("自己改善は設定で無効です。記録済みの候補だけを表示します。");
    screen.get_by_label("cand-a · 未確認 · 警告 · 3 回 · title of cand-a");

    screen
        .get_by_label("cand-a · 未確認 · 警告 · 3 回 · title of cand-a")
        .click();
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
                    run: None,
                    recent_runs: vec![],
                    created_ms: 1,
                    reopened_count: 0,
                }),
            }),
        )))),
    );
    screen.get_by_label("下書き: cand-a.md");
    screen.get_by_label("the tool exited 2");
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
    screen
        .get_by_label("cand-a · 未確認 · 警告 · 3 回 · title of cand-a")
        .click();
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
    screen.get_by_label("却下する");
    screen.get_by_label("下書きを作る").click();
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
    screen.get_by_label_contains("下書き: cand-a-draft.md（送信はされません）");
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
    screen.get_by_label("下書きを作る").click();
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
    screen.get_by_label_contains("成功とは扱いません");
    assert!(screen.query_by_label_contains("下書き:").is_none());
}
