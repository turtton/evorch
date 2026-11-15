//! The improvement feature drawn headless: the list, the settings switch, a
//! candidate's detail with its draft named by file only, and the dropping of an
//! answer for another project.
use super::*;
use crate::core_client::features::execution::tests::{answer, asked, query_of, screen};
use egui_kittest::kittest::Queryable;
use workbench_protocol::{
    ids::OperationId,
    management::ImprovementState,
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
