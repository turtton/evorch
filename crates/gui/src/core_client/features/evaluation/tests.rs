//! The evaluation feature drawn headless: a group that is missing, one that is
//! unusable and one that is there are three different rows, a member's report is
//! present, missing or unavailable, and the screen only ever sends queries.
use super::*;
use crate::core_client::features::execution::tests::{answer, asked, query_of, screen};
use egui_kittest::kittest::Queryable;
use workbench_protocol::{
    ids::OperationId,
    management::EvaluationArtifactKind,
    namespaces::{
        diagnostics::{BoundedText, ReadOutcome},
        evaluation::{EvaluationList, EvaluationMemberSummary, EvaluationTrialCounts},
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

fn group_id(id: &str) -> EvaluationGroupId {
    EvaluationGroupId::new(id).unwrap()
}

fn row(id: &str, presence: ArtifactPresence) -> EvaluationGroupSummary {
    let present = presence == ArtifactPresence::Present;
    EvaluationGroupSummary {
        group: group_id(id),
        presence,
        problem: (presence == ArtifactPresence::Problem)
            .then_some(EvaluationProblemKind::Malformed),
        phase: present.then_some(EvaluationGroupPhase::Finished),
        started_at: present.then(|| "2026-10-20".to_owned()),
        finished_at: None,
        members_total: present.then_some(1),
        revision: Revision(1),
    }
}

fn artifact(
    group: &str,
    member: &str,
    kind: EvaluationArtifactKind,
    presence: ArtifactPresence,
) -> EvaluationArtifactRef {
    EvaluationArtifactRef {
        group: group_id(group),
        member: EvaluationMemberId::new(member).unwrap(),
        kind,
        presence,
    }
}

fn member(group: &str, name: &str, report: ArtifactPresence) -> EvaluationMemberSummary {
    EvaluationMemberSummary {
        member: EvaluationMemberId::new(name).unwrap(),
        index: 0,
        thread_id: "thread-1".into(),
        phase: EvaluationMemberPhase::Finished,
        trials: EvaluationTrialCounts {
            planned: 2,
            queued: 0,
            running: 0,
            finished: 2,
            failed: 0,
            interrupted: 0,
            unavailable: 0,
            unverified: 0,
        },
        report: artifact(group, name, EvaluationArtifactKind::Report, report),
        replay: artifact(
            group,
            name,
            EvaluationArtifactKind::Replay,
            ArtifactPresence::Missing,
        ),
        failure: None,
        problems: 0,
    }
}

#[test]
fn three_kinds_of_group_three_kinds_of_report_and_no_command() {
    let mut screen = screen(Box::<EvaluationFeature>::default(), None);
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    let Query::Evaluation(EvaluationQuery::ListEvaluations(list)) = query_of(&first[0]) else {
        panic!("a list was expected");
    };
    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::List(Box::new(
            ReadOutcome {
                operation: view(&list.operation_id, OperationState::Ready),
                data: Some(EvaluationList {
                    groups: Page {
                        items: vec![
                            row("g-ok", ArtifactPresence::Present),
                            row("g-gone", ArtifactPresence::Missing),
                            row("g-bad", ArtifactPresence::Problem),
                        ],
                        next_cursor: None,
                        snapshot_revision: Revision(1),
                    },
                    omitted: 0,
                    scan_truncated: false,
                    problem: None,
                }),
            },
        ))))),
    );
    screen.get_by_label("g-ok · 完了 · 2026-10-20 · メンバー 1");
    screen.get_by_label("g-gone · 記録なし");
    screen.get_by_label("g-bad · 読み取れません (形式が壊れています)");

    screen
        .get_by_label("g-ok · 完了 · 2026-10-20 · メンバー 1")
        .click();
    let group_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluation(get)) = query_of(&group_ask[0]) else {
        panic!("a group was expected");
    };
    assert_eq!(get.group.as_str(), "g-ok");
    answer(
        &mut screen,
        group_ask[0].tag,
        FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::Group(
            Box::new(ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(EvaluationGroupDetail {
                    group: group_id("g-ok"),
                    phase: EvaluationGroupPhase::Incomplete,
                    started_at: "2026-10-20".into(),
                    finished_at: None,
                    notice: None,
                    candidate: None,
                    members: vec![
                        member("g-ok", "member-a", ArtifactPresence::Present),
                        member("g-ok", "member-b", ArtifactPresence::Problem),
                    ],
                    members_omitted: 0,
                    members_total: 2,
                    revision: Revision(1),
                }),
            }),
        )))),
    );
    screen.get_by_label_contains("member-a · 完了");
    screen.get_by_label_contains("member-b · 完了");
    assert_eq!(
        screen.query_all_by_label_contains("レポート: あり").count(),
        1
    );
    assert_eq!(
        screen
            .query_all_by_label_contains("レポート: 利用不可")
            .count(),
        1
    );

    screen.get_by_label("member-b のレポートを開く").click();
    let report_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluationReport(report)) = query_of(&report_ask[0])
    else {
        panic!("a report was expected");
    };
    assert_eq!(report.member.as_str(), "member-b");
    answer(
        &mut screen,
        report_ask[0].tag,
        FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::Report(
            Box::new(ReadOutcome {
                operation: view(&report.operation_id, OperationState::Ready),
                data: Some(EvaluationReportText {
                    artifact: artifact(
                        "g-ok",
                        "member-b",
                        EvaluationArtifactKind::Report,
                        ArtifactPresence::Problem,
                    ),
                    text: None,
                    problem: Some(EvaluationProblemKind::UnsafePath),
                }),
            }),
        )))),
    );
    screen.get_by_label("レポートは利用できません (安全でないパスです)。");
}

#[test]
fn a_present_report_shows_its_cut_text() {
    let mut screen = screen(Box::<EvaluationFeature>::default(), None);
    let first = asked(&mut screen);
    // Select a group and a member the way a click does, then answer the report.
    {
        let feature = screen.state_mut().feature.as_mut();
        let _ = feature;
    }
    let Query::Evaluation(EvaluationQuery::ListEvaluations(list)) = query_of(&first[0]) else {
        panic!("a list was expected");
    };
    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::List(Box::new(
            ReadOutcome {
                operation: view(&list.operation_id, OperationState::Ready),
                data: Some(EvaluationList {
                    groups: Page {
                        items: vec![row("g-ok", ArtifactPresence::Present)],
                        next_cursor: None,
                        snapshot_revision: Revision(1),
                    },
                    omitted: 0,
                    scan_truncated: false,
                    problem: None,
                }),
            },
        ))))),
    );
    screen
        .get_by_label("g-ok · 完了 · 2026-10-20 · メンバー 1")
        .click();
    let group_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluation(get)) = query_of(&group_ask[0]) else {
        panic!("a group was expected");
    };
    answer(
        &mut screen,
        group_ask[0].tag,
        FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::Group(
            Box::new(ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(EvaluationGroupDetail {
                    group: group_id("g-ok"),
                    phase: EvaluationGroupPhase::Finished,
                    started_at: "2026-10-20".into(),
                    finished_at: None,
                    notice: None,
                    candidate: None,
                    members: vec![member("g-ok", "member-a", ArtifactPresence::Present)],
                    members_omitted: 0,
                    members_total: 1,
                    revision: Revision(1),
                }),
            }),
        )))),
    );
    screen.get_by_label("member-a のレポートを開く").click();
    let report_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluationReport(report)) = query_of(&report_ask[0])
    else {
        panic!("a report was expected");
    };
    answer(
        &mut screen,
        report_ask[0].tag,
        FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::Report(
            Box::new(ReadOutcome {
                operation: view(&report.operation_id, OperationState::Ready),
                data: Some(EvaluationReportText {
                    artifact: artifact(
                        "g-ok",
                        "member-a",
                        EvaluationArtifactKind::Report,
                        ArtifactPresence::Present,
                    ),
                    text: Some(BoundedText {
                        text: "# Report".into(),
                        total_bytes: 9000,
                        truncated: true,
                    }),
                    problem: None,
                }),
            }),
        )))),
    );
    screen.get_by_label("# Report");
    screen.get_by_label("(先頭 8 バイトのみ表示 / 全体 9000 バイト)");
}
