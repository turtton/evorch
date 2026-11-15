//! The diagnostics feature drawn headless: the usage summary is asked as an
//! operation and shows a cost as a lower bound when something had no price, and a
//! run's saved context needs a well-formed run id before anything is asked.
use super::*;
use crate::core_client::features::execution::tests::{answer, asked, query_of, screen};
use egui_kittest::kittest::Queryable;
use workbench_protocol::{
    namespaces::diagnostics::ReadOutcome,
    operation::{OperationState, OperationView},
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

fn totals(unpriced: u64) -> UsageTotals {
    UsageTotals {
        requests: 4,
        failed: 1,
        input_tokens: 300,
        output_tokens: 30,
        recorded_cost_micro_usd: 1_500_000,
        unpriced_tokens: unpriced,
        ..Default::default()
    }
}

#[test]
fn the_usage_summary_is_asked_once_and_an_unpriced_total_is_a_lower_bound() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    let Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(get)) = query_of(&first[0]) else {
        panic!("a usage summary was expected");
    };
    assert_eq!(get.range, UsageRange::Last7Days);

    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(
            Box::new(ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(UsageSummary {
                    range: UsageRange::Last7Days,
                    from_day: "2026-10-14".into(),
                    to_day: "2026-10-20".into(),
                    totals: totals(110),
                    days: vec![],
                    days_omitted: 0,
                    models: vec![],
                    models_omitted: 0,
                }),
            }),
        )))),
    );
    screen.get_by_label("直近 7 日 — 2026-10-14 から 2026-10-20");
    screen.get_by_label_contains(
        "記録された費用 $1.500000 (下限: 価格のないトークン 110 / 使用量未報告 0 件)",
    );
    assert!(asked(&mut screen).is_empty(), "nothing is asked twice");

    screen.get_by_label("再読み込み").click();
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    let Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(next)) = query_of(&again[0]) else {
        panic!("a usage summary was expected");
    };
    assert_ne!(next.operation_id, get.operation_id);
}

#[test]
fn a_malformed_run_id_asks_nothing_and_says_so() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let _ = asked(&mut screen);
    screen.state_mut().feature = Box::new(DiagnosticsFeature {
        run_input: "not a run id!".into(),
        ..Default::default()
    });
    let _ = asked(&mut screen);
    screen.get_by_label("開く").click();
    let after = asked(&mut screen);
    assert!(
        after.iter().all(|action| !matches!(
            query_of(action),
            Query::Diagnostics(DiagnosticsQuery::GetRunContext(_))
        )),
        "no context ask for a malformed id"
    );
    screen.get_by_label("run id の形式が正しくありません。");
}

#[test]
fn dollars_keep_every_micro_dollar() {
    assert_eq!(dollars(0), "$0.000000");
    assert_eq!(dollars(1), "$0.000001");
    assert_eq!(dollars(12_345_678), "$12.345678");
}

#[test]
fn a_run_id_is_the_numbered_or_the_26_character_form_and_nothing_else() {
    assert!(run_id(" run-12 ").is_some());
    assert!(run_id("run-01m7g3rpyjwtpb574dsps9c462").is_some());
    for bad in [
        "",
        "run-",
        "run-1a",
        "12",
        "run-01m7g3rpyjwtpb574dsps9c46",
        "run-01m7g3rpyjwtpb574dsps9c46u",
    ] {
        assert!(run_id(bad).is_none(), "{bad:?}");
    }
}
