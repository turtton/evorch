//! The cost of the selected thread in the conversation's header and status line, and
//! the Usage tab's Live view, in the whole window.
use super::*;
use workbench_protocol::{
    namespaces::diagnostics::{
        DiagnosticsQuery, DiagnosticsReply, GetLiveUsage, LIVE_USAGE_MINUTES, LiveUsage,
        ReadOutcome, ThreadUsage, UsageTotals, UsageValue,
    },
    operation::{OperationState, OperationView},
};

fn ready(operation_id: workbench_protocol::ids::OperationId) -> OperationView {
    OperationView {
        operation_id,
        state: OperationState::Ready,
        revision: Revision(1),
        result: None,
    }
}

fn diagnostics_asked(actions: impl IntoIterator<Item = Action>) -> Vec<(u64, DiagnosticsQuery)> {
    actions
        .into_iter()
        .filter_map(|action| match action {
            Action::Feature(FeatureAction {
                feature: FeatureId::Diagnostics,
                tag,
                request: FeatureRequest::Query(Query::Diagnostics(query)),
            }) => Some((tag, query)),
            _ => None,
        })
        .collect()
}

fn drain(screen: &mut Screen) -> Vec<Action> {
    screen.harness.run_steps(2);
    let mut actions = Vec::new();
    while let Ok(action) = screen.actions.try_recv() {
        actions.push(action);
    }
    actions
}

fn value(requests: u64, micro_usd: u64, incomplete: bool) -> UsageValue {
    UsageValue {
        totals: UsageTotals {
            requests,
            input_tokens: 1_000 * requests,
            recorded_cost_micro_usd: micro_usd,
            ..Default::default()
        },
        incomplete,
    }
}

fn reply(screen: &mut Screen, tag: u64, reply: DiagnosticsReply) {
    screen.shared.lock().unwrap().features.reply(
        FeatureId::Diagnostics,
        tag,
        FeatureReply::Query(Ok(QueryReply::Diagnostics(reply))),
    );
    screen.harness.run_steps(2);
}

fn live() -> LiveUsage {
    let spent = |requests: u64, micro: u64| UsageTotals {
        requests,
        input_tokens: 4_000 * requests,
        output_tokens: 300 * requests,
        cache_read_tokens: 2_500 * requests,
        recorded_cost_micro_usd: micro,
        ..Default::default()
    };
    let mut minutes = vec![UsageTotals::default(); LIVE_USAGE_MINUTES];
    for (ago, requests, micro) in [
        (1, 2, 90_000),
        (3, 1, 40_000),
        (4, 3, 120_000),
        (9, 1, 35_000),
        (14, 2, 80_000),
        (22, 4, 190_000),
        (31, 1, 30_000),
        (47, 2, 85_000),
        (58, 3, 110_000),
        (96, 2, 70_000),
        (140, 1, 45_000),
    ] {
        minutes[LIVE_USAGE_MINUTES - ago] = spent(requests, micro);
    }
    let day_start_ms = 1_788_048_000_000;
    LiveUsage {
        as_of_ms: day_start_ms + 14 * 3_600_000,
        minutes,
        local_day: "2026-08-30".into(),
        day_start_ms,
        day_end_ms: day_start_ms + 24 * 3_600_000,
        today: spent(22, 895_000),
        month: spent(310, 12_400_000),
        quota_windows: Vec::new(),
    }
}

// Given: the selected thread, which the core knows
// When: the core answers its usage with a lower bound for the whole thread
// Then: the header reads the whole thread's cost, the status line the main agent's, and before
//       the answer both read a dash
#[test]
fn the_header_and_status_line_show_the_cost_of_the_selected_thread() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let asked = diagnostics_asked(screen.settle());
    let (tag, get) = asked
        .into_iter()
        .find_map(|(tag, query)| match query {
            DiagnosticsQuery::GetThreadUsage(get) => Some((tag, get)),
            _ => None,
        })
        .expect("the usage of the selected thread was asked");
    assert_eq!(get.thread.as_str(), "thr-1");
    screen
        .harness
        .get_by_label("Total cost (incl. subagents) $—");
    screen.harness.get_by_label("$—");

    reply(
        &mut screen,
        tag,
        DiagnosticsReply::ThreadUsage(Box::new(ReadOutcome {
            operation: ready(get.operation_id),
            data: Some(ThreadUsage {
                thread: get.thread,
                total: value(14, 2_500_000, true),
                conversation: value(9, 750_000, false),
            }),
        })),
    );
    screen
        .harness
        .get_by_label("Total cost (incl. subagents) $2.50*");
    screen.harness.get_by_label("$0.75");
    screen.capture("conversation-cost");
}

// Given: the Usage tab opened from the footer
// When: Live is chosen and the core answers
// Then: the live usage is asked once, drawn with the pace and forecasts, and no Refresh is offered
#[test]
fn the_usage_tab_draws_the_live_view_and_offers_no_refresh_in_it() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.get_by_label("Usage").click();
    drain(&mut screen);
    assert!(screen.harness.query_by_label("Refresh").is_none());
    screen
        .harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Live")
        .click();
    let asked = diagnostics_asked(drain(&mut screen));
    let lives: Vec<(u64, GetLiveUsage)> = asked
        .into_iter()
        .filter_map(|(tag, query)| match query {
            DiagnosticsQuery::GetLiveUsage(get) => Some((tag, get)),
            _ => None,
        })
        .collect();
    let [(tag, get)] = lives.as_slice() else {
        panic!("one live read was expected, got {}", lives.len());
    };
    reply(
        &mut screen,
        *tag,
        DiagnosticsReply::LiveUsage(Box::new(ReadOutcome {
            operation: ready(get.operation_id.clone()),
            data: Some(live()),
        })),
    );
    for label in [
        "Burn rate",
        "Requests per minute",
        "Tokens per minute",
        "$12.40",
    ] {
        screen.harness.get_by_label(label);
    }
    assert!(screen.harness.query_by_label("Refresh").is_none());
    screen.capture("usage-live");
}
