//! The diagnostics feature drawn headless: the usage summary is asked as an
//! operation and shows a cost as a lower bound when something had no price, and a
//! run's saved context needs a well-formed run id before anything is asked.
use super::*;
use crate::core_client::cache::Cache;
use crate::core_client::features::execution::tests::{answer, asked, query_of, screen};
use crate::core_client::features::{FeatureAction, FeatureFrame, FeatureView, Selection};
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

fn summary_of(action: &FeatureAction) -> &GetUsageSummary {
    let Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(get)) = query_of(action) else {
        panic!("a usage summary was expected");
    };
    get
}

fn usage(get: &GetUsageSummary, state: OperationState, unpriced: u64) -> FeatureReply {
    let data = (state == OperationState::Ready).then(|| UsageSummary {
        range: get.range,
        from_day: "2026-10-14".into(),
        to_day: "2026-10-20".into(),
        totals: totals(unpriced),
        days: vec![],
        days_omitted: 0,
        models: vec![],
        models_omitted: 0,
    });
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(
        Box::new(ReadOutcome {
            operation: view(&get.operation_id, state),
            data,
        }),
    ))))
}

fn maintain_at(
    feature: &mut DiagnosticsFeature,
    available: bool,
    now: Instant,
) -> Vec<FeatureAction> {
    let capabilities = [Capability::Queries];
    let cache = Cache::default();
    let selection = Selection::default();
    let mut frame = FeatureFrame::default();
    let mut ctx = FeatureContext {
        data: &mut super::super::data::DataInbox::default(),
        feature: FeatureId::Diagnostics,
        view: FeatureView {
            connected: available,
            capabilities: &capabilities,
            cache: &cache,
            selection: &selection,
        },
        frame: &mut frame,
    };
    let visibility = Visibility {
        available,
        expanded: true,
        minimized: false,
    };
    feature.keep_today(&mut ctx, visibility, now);
    frame.actions
}

#[test]
fn the_usage_summary_is_asked_once_and_an_unpriced_total_is_a_lower_bound() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let first = asked(&mut screen);
    let ranges: Vec<_> = first
        .iter()
        .map(|action| summary_of(action).range)
        .collect();
    assert_eq!(ranges, [UsageRange::Today, UsageRange::Last7Days]);
    let get = summary_of(&first[1]);

    answer(
        &mut screen,
        first[1].tag,
        usage(get, OperationState::Ready, 110),
    );
    screen.get_by_label("直近 7 日 — 2026-10-14 から 2026-10-20");
    screen.get_by_label_contains(
        "記録された費用 $1.500000 (下限: 価格のないトークン 110 / 使用量未報告 0 件)",
    );
    assert!(asked(&mut screen).is_empty(), "nothing is asked twice");

    screen.get_by_label("再読み込み").click();
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert_ne!(summary_of(&again[0]).operation_id, get.operation_id);
}

// Given: the first pass of an available feature, whose tab is not drawn
// When: the core answers today's summary, with unpriced tokens
// Then: today is asked under an operation of its own, and the footer's cost is a lower bound
#[test]
fn todays_cost_is_read_apart_from_the_tab_and_marked_as_a_lower_bound() {
    let mut feature = DiagnosticsFeature::default();
    let t0 = Instant::now();
    let first = maintain_at(&mut feature, true, t0);
    assert_eq!(first.len(), 1);
    let today = summary_of(&first[0]).clone();
    assert_eq!(today.range, UsageRange::Today);
    assert!(
        today
            .operation_id
            .as_str()
            .starts_with("diagnostics-today-")
    );
    assert_eq!(feature.today_cost(), None);

    feature.reply(first[0].tag, usage(&today, OperationState::Ready, 110));
    assert_eq!(feature.today_cost().as_deref(), Some("$1.50*"));
    assert_eq!(feature.today_refresh_in(t0), Some(TODAY_REFRESH));
}

// Given: today's summary answered Pending
// When: the area's event says the operation settled
// Then: the same request is asked again, once, and its Ready answer gives the cost
#[test]
fn a_settled_today_read_is_asked_again_once() {
    let mut feature = DiagnosticsFeature::default();
    let t0 = Instant::now();
    let first = maintain_at(&mut feature, true, t0);
    let today = summary_of(&first[0]).clone();
    feature.reply(first[0].tag, usage(&today, OperationState::Pending, 0));
    assert!(maintain_at(&mut feature, true, t0).is_empty());

    feature.event(&UiEvent::Diagnostics(
        DiagnosticsEvent::DiagnosticsOperationChanged {
            view: view(&today.operation_id, OperationState::Ready),
        },
    ));
    let again = maintain_at(&mut feature, true, t0);
    assert_eq!(again.len(), 1);
    assert_eq!(summary_of(&again[0]), &today);
    assert!(maintain_at(&mut feature, true, t0).is_empty());

    feature.reply(again[0].tag, usage(&today, OperationState::Ready, 0));
    assert_eq!(feature.today_cost().as_deref(), Some("$1.50"));
}

// Given: today's cost was asked / When: passes run before and at the refresh time
// Then: nothing is asked before it, and a new read is asked at it
#[test]
fn todays_cost_is_refreshed_every_minute() {
    let mut feature = DiagnosticsFeature::default();
    let t0 = Instant::now();
    let first = maintain_at(&mut feature, true, t0);
    let before = t0 + TODAY_REFRESH - Duration::from_millis(1);
    assert!(maintain_at(&mut feature, true, before).is_empty());
    assert_eq!(
        feature.today_refresh_in(before),
        Some(Duration::from_millis(1))
    );
    let refreshed = maintain_at(&mut feature, true, t0 + TODAY_REFRESH);
    assert_eq!(refreshed.len(), 1);
    let next = summary_of(&refreshed[0]);
    assert_eq!(next.range, UsageRange::Today);
    assert_ne!(next.operation_id, summary_of(&first[0]).operation_id);
}

// Given: a known cost of today / When: a new snapshot resets the feature
// Then: the cost is gone and asked again at once; an unavailable feature asks nothing
#[test]
fn a_reset_forgets_todays_cost_and_an_unavailable_feature_asks_nothing() {
    let mut feature = DiagnosticsFeature::default();
    let t0 = Instant::now();
    assert!(maintain_at(&mut feature, false, t0).is_empty());
    assert_eq!(feature.today_refresh_in(t0), None);

    let first = maintain_at(&mut feature, true, t0);
    let today = summary_of(&first[0]).clone();
    feature.reply(first[0].tag, usage(&today, OperationState::Ready, 0));
    assert!(feature.today_cost().is_some());

    feature.reset();
    assert_eq!(feature.today_cost(), None);
    let again = maintain_at(&mut feature, true, t0);
    assert_eq!(again.len(), 1);
    feature.reply(first[0].tag, usage(&today, OperationState::Ready, 0));
    assert_eq!(
        feature.today_cost(),
        None,
        "an answer from before the reset is dropped"
    );
}

#[test]
fn the_core_judges_a_run_id_and_its_refusal_is_shown_under_the_input() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let _ = asked(&mut screen);
    screen.state_mut().feature = Box::new(DiagnosticsFeature {
        run_input: " not a run id! ".into(),
        ..Default::default()
    });
    let _ = asked(&mut screen);
    assert!(
        screen
            .query_by_role(egui::accesskit::Role::TextInput)
            .is_none(),
        "the input is shut until it is asked for"
    );
    screen.get_by_label("run id を指定して開く").click();
    screen.run();
    screen.get_by_label("開く").click();
    let after = asked(&mut screen);
    let Query::Diagnostics(DiagnosticsQuery::GetRunContext(get)) = query_of(&after[0]) else {
        panic!("a context ask was expected");
    };
    assert_eq!(
        get.run.as_str(),
        "not a run id!",
        "what was typed goes to the core"
    );
    answer(
        &mut screen,
        after[0].tag,
        FeatureReply::Query(Err(crate::core_client::features::FeatureError::new(
            "not a run id",
        ))),
    );
    screen.get_by_label("not a run id");
}

#[test]
fn nothing_typed_asks_nothing() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let _ = asked(&mut screen);
    screen.get_by_label("run id を指定して開く").click();
    screen.run();
    screen.get_by_label("開く").click();
    assert!(asked(&mut screen).iter().all(|action| !matches!(
        query_of(action),
        Query::Diagnostics(DiagnosticsQuery::GetRunContext(_))
    )));
}

#[test]
fn dollars_keep_every_micro_dollar() {
    assert_eq!(dollars(0), "$0.000000");
    assert_eq!(dollars(1), "$0.000001");
    assert_eq!(dollars(12_345_678), "$12.345678");
}
