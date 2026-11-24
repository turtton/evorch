//! The evaluation feature drawn headless: a group that is missing, one that is
//! unusable and one that is there are three different rows, a member's report is
//! present, missing or unavailable, the pages follow the legacy panel's breadcrumb,
//! and the screen only ever sends queries.
use super::*;
use crate::core_client::features::execution::tests::{Screen, answer, asked, query_of, screen};
use egui_kittest::Harness;
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

fn list_answer(id: &OperationId, items: Vec<EvaluationGroupSummary>) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::List(Box::new(
        ReadOutcome {
            operation: view(id, OperationState::Ready),
            data: Some(EvaluationList {
                groups: Page {
                    items,
                    next_cursor: None,
                    snapshot_revision: Revision(1),
                },
                omitted: 0,
                scan_truncated: false,
                problem: None,
            }),
        },
    )))))
}

fn group_answer(id: &OperationId, detail: EvaluationGroupDetail) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::Group(
        Box::new(ReadOutcome {
            operation: view(id, OperationState::Ready),
            data: Some(detail),
        }),
    ))))
}

fn detail(
    phase: EvaluationGroupPhase,
    members: Vec<EvaluationMemberSummary>,
) -> EvaluationGroupDetail {
    EvaluationGroupDetail {
        group: group_id("g-ok"),
        phase,
        started_at: "2026-10-20".into(),
        finished_at: None,
        notice: None,
        candidate: None,
        members_omitted: 0,
        members_total: members.len() as u32,
        members,
        revision: Revision(1),
    }
}

/// A screen listing `g-ok`, with `g-ok` clicked: returns the group ask.
fn screen_with_group_asked() -> (Harness<'static, Screen>, u64, GetEvaluation) {
    let mut screen = screen(Box::<EvaluationFeature>::default(), None);
    let first = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::ListEvaluations(list)) = query_of(&first[0]) else {
        panic!("a list was expected");
    };
    answer(
        &mut screen,
        first[0].tag,
        list_answer(
            &list.operation_id,
            vec![row("g-ok", ArtifactPresence::Present)],
        ),
    );
    screen.get_by_label("g-ok").click();
    let group_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluation(get)) = query_of(&group_ask[0]) else {
        panic!("a group was expected");
    };
    let get = get.clone();
    (screen, group_ask[0].tag, get)
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
        list_answer(
            &list.operation_id,
            vec![
                row("g-ok", ArtifactPresence::Present),
                row("g-gone", ArtifactPresence::Missing),
                row("g-bad", ArtifactPresence::Problem),
            ],
        ),
    );
    screen.get_by_label("g-ok");
    screen.get_by_label("Finished");
    screen.get_by_label("2026-10-20 · 1 threads");
    screen.get_by_label("g-gone");
    screen.get_by_label("record missing");
    screen.get_by_label("g-bad");
    screen.get_by_label("Artifact problem: malformed");

    screen.get_by_label("g-ok").click();
    let group_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluation(get)) = query_of(&group_ask[0]) else {
        panic!("a group was expected");
    };
    assert_eq!(get.group.as_str(), "g-ok");
    answer(
        &mut screen,
        group_ask[0].tag,
        group_answer(
            &get.operation_id,
            detail(
                EvaluationGroupPhase::Incomplete,
                vec![
                    member("g-ok", "member-a", ArtifactPresence::Present),
                    member("g-ok", "member-b", ArtifactPresence::Problem),
                ],
            ),
        ),
    );
    // The group page replaces the list, as in the legacy panel.
    assert!(screen.query_by_label("g-gone").is_none());
    screen.get_by_label("Incomplete");
    screen.get_by_label("Threads");
    screen.get_by_label("member-a · thread-1");
    screen.get_by_label("member-b · thread-1");
    assert_eq!(screen.query_all_by_label("Finished").count(), 2);
    assert_eq!(
        screen
            .query_all_by_label_contains("2 of 2 trials finished")
            .count(),
        2
    );
    assert_eq!(
        screen
            .query_all_by_label_contains("Report: available")
            .count(),
        1
    );
    assert_eq!(
        screen
            .query_all_by_label_contains("Report: unusable")
            .count(),
        1
    );

    screen.get_by_label("member-b · thread-1").click();
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
    screen.get_by_label("Artifact problem: unsafe_path");
    assert!(screen.query_by_label("Threads").is_none());
}

#[test]
fn a_present_report_shows_its_cut_text() {
    let (mut screen, tag, get) = screen_with_group_asked();
    answer(
        &mut screen,
        tag,
        group_answer(
            &get.operation_id,
            detail(
                EvaluationGroupPhase::Finished,
                vec![member("g-ok", "member-a", ArtifactPresence::Present)],
            ),
        ),
    );
    screen.get_by_label("member-a · thread-1").click();
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
                        text: "# Summary\n\nthe candidate won".into(),
                        total_bytes: 9000,
                        truncated: true,
                    }),
                    problem: None,
                }),
            }),
        )))),
    );
    screen.get_by_label("Report");
    screen.get_by_label_contains("the candidate won");
    screen.get_by_label("Showing the start of report.md (28 of 9000 bytes).");
}

#[test]
fn the_groups_link_goes_back_and_a_late_group_answer_is_dropped() {
    let (mut screen, tag, get) = screen_with_group_asked();
    screen.get_by_label("Loading…");
    screen.get_by_label("Groups").click();
    screen.run();
    screen.get_by_label("2026-10-20 · 1 threads");
    assert!(screen.query_by_label("Groups").is_none());

    // The group was forgotten with the page: its answer does not bring the page back.
    answer(
        &mut screen,
        tag,
        group_answer(
            &get.operation_id,
            detail(
                EvaluationGroupPhase::Finished,
                vec![member("g-ok", "member-a", ArtifactPresence::Present)],
            ),
        ),
    );
    assert!(screen.query_by_label("member-a · thread-1").is_none());
    screen.get_by_label("2026-10-20 · 1 threads");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn an_unverified_group_says_what_unverified_means_and_its_candidate_folds() {
    let (mut screen, tag, get) = screen_with_group_asked();
    let mut unverified = detail(
        EvaluationGroupPhase::Unverified,
        vec![member("g-ok", "member-a", ArtifactPresence::Missing)],
    );
    unverified.candidate = Some(BoundedText {
        text: "model = \"candidate\"".into(),
        total_bytes: 4096,
        truncated: true,
    });
    answer(
        &mut screen,
        tag,
        group_answer(&get.operation_id, unverified),
    );
    screen.get_by_label("Unverified");
    screen.get_by_label(crate::panes::evaluation_widgets::UNVERIFIED_HINT);
    assert!(screen.query_by_label("model = \"candidate\"").is_none());
    screen.get_by_label("Candidate configuration").click();
    screen.run();
    screen.get_by_label("model = \"candidate\"");
    screen.get_by_label("Showing the start of 4096 bytes.");

    // A member's page, then the group's link in the breadcrumb, back to the threads.
    screen.get_by_label("member-a · thread-1").click();
    asked(&mut screen);
    screen.get_by_label("Report");
    // The breadcrumb and the heading both name the thread.
    assert_eq!(screen.query_all_by_label("thread-1").count(), 2);
    screen.get_by_label("g-ok").click();
    screen.run();
    screen.get_by_label("Threads");
}

fn report_answer(id: &OperationId, text: &str) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Evaluation(EvaluationReply::Report(
        Box::new(ReadOutcome {
            operation: view(id, OperationState::Ready),
            data: Some(EvaluationReportText {
                artifact: artifact(
                    "g-ok",
                    "member-a",
                    EvaluationArtifactKind::Report,
                    ArtifactPresence::Present,
                ),
                text: Some(BoundedText {
                    text: text.into(),
                    total_bytes: text.len() as u64,
                    truncated: false,
                }),
                problem: None,
            }),
        }),
    ))))
}

#[test]
fn refresh_on_a_report_page_re_reads_the_group_and_the_report_and_stays() {
    let (mut screen, tag, get) = screen_with_group_asked();
    let finished = || {
        detail(
            EvaluationGroupPhase::Finished,
            vec![member("g-ok", "member-a", ArtifactPresence::Present)],
        )
    };
    answer(
        &mut screen,
        tag,
        group_answer(&get.operation_id, finished()),
    );
    screen.get_by_label("member-a · thread-1").click();
    let report_ask = asked(&mut screen);
    let Query::Evaluation(EvaluationQuery::GetEvaluationReport(report)) = query_of(&report_ask[0])
    else {
        panic!("a report was expected");
    };
    answer(
        &mut screen,
        report_ask[0].tag,
        report_answer(&report.operation_id, "first reading"),
    );
    screen.get_by_label_contains("first reading");

    screen.get_by_label("Refresh").click();
    let again = asked(&mut screen);
    let group_again = again
        .iter()
        .find_map(|ask| match query_of(ask) {
            Query::Evaluation(EvaluationQuery::GetEvaluation(get)) => Some((ask.tag, get.clone())),
            _ => None,
        })
        .expect("the group is re-read");
    let report_again = again
        .iter()
        .find_map(|ask| match query_of(ask) {
            Query::Evaluation(EvaluationQuery::GetEvaluationReport(report)) => {
                Some((ask.tag, report.clone()))
            }
            _ => None,
        })
        .expect("the report is re-read");
    assert_eq!(report_again.1.member.as_str(), "member-a");
    // While both are in flight the page stays the member's, never the threads list.
    screen.get_by_label("Report");
    assert!(screen.query_by_label("Threads").is_none());

    answer(
        &mut screen,
        group_again.0,
        group_answer(&group_again.1.operation_id, finished()),
    );
    answer(
        &mut screen,
        report_again.0,
        report_answer(&report_again.1.operation_id, "second reading"),
    );
    screen.get_by_label_contains("second reading");
    assert!(screen.query_by_label("Threads").is_none());
    assert_eq!(screen.query_all_by_label("thread-1").count(), 2);
}
