//! The Usage tab drawn headless: the usage summary is asked as an operation and drawn
//! as the legacy Usage tab draws it, a cost is a lower bound when something had no
//! price, and the Context view reads a run of the selected thread.
use super::*;
use crate::core_client::cache::Cache;
use crate::core_client::features::execution::tests::{Screen, answer, query_of, screen};
use crate::core_client::features::{
    FeatureAction, FeatureError, FeatureFrame, FeatureView, Selection,
};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::{
    ids::{PageCursor, ThreadId},
    namespaces::diagnostics::{
        ContextMessageRole, ContextSegment, ReadOutcome, RunContext, UsageDay, UsageModel,
        UsageSummary,
    },
    operation::{OperationState, OperationView},
    views::Page,
    wire::Revision,
};

/// Usage/context assertions are independent of the footer's automatic quota read.
fn asked(screen: &mut Harness<'_, Screen>) -> Vec<FeatureAction> {
    crate::core_client::features::execution::tests::asked(screen)
        .into_iter()
        .filter(|action| {
            !matches!(
                query_of(action),
                Query::Diagnostics(DiagnosticsQuery::GetProviderQuotas(_))
            )
        })
        .collect()
}

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

fn spent(requests: u64, micro_usd: u64) -> UsageTotals {
    UsageTotals {
        requests,
        input_tokens: 100,
        output_tokens: 10,
        recorded_cost_micro_usd: micro_usd,
        ..Default::default()
    }
}

fn summary_of(action: &FeatureAction) -> &GetUsageSummary {
    let Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(get)) = query_of(action) else {
        panic!("a usage summary was expected");
    };
    get
}

fn context_of(action: &FeatureAction) -> &GetRunContext {
    let Query::Diagnostics(DiagnosticsQuery::GetRunContext(get)) = query_of(action) else {
        panic!("a run context was expected");
    };
    get
}

fn usage(get: &GetUsageSummary, state: OperationState, unpriced: u64) -> FeatureReply {
    let data = (state == OperationState::Ready).then(|| UsageSummary {
        range: get.range,
        from_day: "2026-10-14".into(),
        to_day: "2026-10-20".into(),
        totals: totals(unpriced),
        days: vec![
            UsageDay {
                day: "2026-10-15".into(),
                totals: spent(3, 1_000_000),
            },
            UsageDay {
                day: "2026-10-19".into(),
                totals: spent(1, 500_000),
            },
        ],
        days_omitted: 0,
        models: vec![
            UsageModel {
                provider: "anthropic".into(),
                model: "opus".into(),
                totals: spent(2, 1_000_000),
            },
            UsageModel {
                provider: "anthropic".into(),
                model: "haiku".into(),
                totals: spent(1, 200_000),
            },
            UsageModel {
                provider: "openai".into(),
                model: "gpt".into(),
                totals: spent(1, 300_000),
            },
        ],
        models_omitted: 0,
    });
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(
        Box::new(ReadOutcome {
            operation: view(&get.operation_id, state),
            data,
        }),
    ))))
}

fn steps(screen: &mut Harness<'_, Screen>) {
    screen.run_steps(3);
}

fn combo_values(screen: &Harness<'_, Screen>) -> Vec<String> {
    screen
        .get_all_by_role(Role::ComboBox)
        .map(|node| node.accesskit_node().value().unwrap_or_default())
        .collect()
}

fn pick(screen: &mut Harness<'_, Screen>, combo: usize, choice: &str) {
    screen
        .get_all_by_role(Role::ComboBox)
        .nth(combo)
        .expect("the combo box is drawn")
        .click();
    steps(screen);
    screen.get_by_role_and_label(Role::Button, choice).click();
    steps(screen);
}

fn answered(thread: Option<&str>) -> Harness<'static, Screen> {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), thread);
    let first = asked(&mut screen);
    let get = summary_of(&first[1]).clone();
    answer(
        &mut screen,
        first[1].tag,
        usage(&get, OperationState::Ready, 110),
    );
    screen
}

// Given: the first pass of the tab / When: the core answers the last 7 days
// Then: the Overview shows the range, the tiles with a lower-bound cost and the legacy charts
#[test]
fn the_usage_summary_is_asked_once_and_drawn_as_the_legacy_overview() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let first = asked(&mut screen);
    let ranges: Vec<_> = first
        .iter()
        .map(|action| summary_of(action).range)
        .collect();
    assert_eq!(ranges, [UsageRange::Today, UsageRange::Last7Days]);
    let get = summary_of(&first[1]).clone();
    screen.get_by_label("Loading usage…");

    answer(
        &mut screen,
        first[1].tag,
        usage(&get, OperationState::Ready, 110),
    );
    screen.get_by_label("Usage");
    assert_eq!(combo_values(&screen)[0], "Last 7 days");
    screen.get_by_label("2026-10-14 to 2026-10-20");
    screen.get_by_role_and_label(Role::Button, "Overview");
    screen.get_by_label("$1.50*");
    screen.get_by_label("75.0% succeeded");
    for title in ["Daily tokens", "Daily cost", "Activity", "Distribution"] {
        screen.get_by_label(title);
    }
    screen.get_by_label("opus");
    assert!(asked(&mut screen).is_empty(), "nothing is asked twice");

    screen.get_by_label("Refresh").click();
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert_ne!(summary_of(&again[0]).operation_id, get.operation_id);
}

// Given: an answered tab / When: every range is picked in turn
// Then: the combo offers the legacy words, and each pick asks for its range
#[test]
fn every_range_is_offered_in_the_legacy_words_and_asked_when_picked() {
    let mut screen = answered(None);
    for (range, label) in [
        (UsageRange::Today, "Today"),
        (UsageRange::Last30Days, "Last 30 days"),
        (UsageRange::ThisMonth, "This month"),
        (UsageRange::Last90Days, "Last 90 days"),
        (UsageRange::AllTime, "All time"),
        (UsageRange::Last7Days, "Last 7 days"),
    ] {
        pick(&mut screen, 0, label);
        let asked = asked(&mut screen);
        assert_eq!(asked.len(), 1, "{label}");
        assert_eq!(summary_of(&asked[0]).range, range);
        assert_eq!(combo_values(&screen)[0], label);
    }
}

fn painted_texts(screen: &Harness<'_, Screen>) -> Vec<String> {
    fn collect(shape: &egui::Shape, into: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => into.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, into)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    for clipped in &screen.output().shapes {
        collect(&clipped.shape, &mut texts);
    }
    texts
}

fn painted_rects(screen: &Harness<'_, Screen>) -> Vec<egui::epaint::RectShape> {
    screen
        .output()
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Rect(rect) => Some(rect.clone()),
            _ => None,
        })
        .collect()
}

/// Panics on any Japanese text painted or exposed to accessibility.
fn assert_english(screen: &Harness<'_, Screen>) {
    let japanese = |text: &str| {
        text.chars().any(|c| {
            matches!(c, '\u{3040}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
        })
    };
    for text in painted_texts(screen) {
        assert!(!japanese(&text), "painted: {text}");
    }
    for node in screen.query_all_by(|_| true) {
        let node = node.accesskit_node();
        for text in [node.label(), node.value()].into_iter().flatten() {
            assert!(!japanese(&text), "accessible: {text}");
        }
    }
}

// Given: a summary whose days are sparse and whose models cost differently
// When: the Overview is drawn
// Then: the charts span every day of the range, the cost bars follow the daily cost, the
//       activity grid marks the days with usage, and the distribution splits the cost by model
#[test]
fn the_overview_charts_are_fed_from_the_days_and_models_of_the_summary() {
    let screen = answered(None);
    let texts = painted_texts(&screen);
    for day in ["10-14", "10-15", "10-17", "10-20"] {
        assert!(texts.iter().any(|text| text == day), "axis label {day}");
    }

    let series = crate::panes::usage_widgets::SERIES[0];
    let bars: Vec<f32> = painted_rects(&screen)
        .into_iter()
        .filter(|rect| rect.stroke.color == series)
        .map(|rect| rect.rect.height())
        .collect();
    assert_eq!(
        bars.len(),
        14,
        "seven uncached-input bars, then seven cost bars"
    );
    let cost = &bars[7..];
    let drawn: Vec<usize> = (0..7).filter(|day| cost[*day] > 1.0).collect();
    assert_eq!(drawn, [1, 5], "2026-10-15 and 2026-10-19");
    assert!(
        (cost[1] / cost[5] - 2.0).abs() < 0.05,
        "$1.00 against $0.50"
    );

    let busiest = crate::panes::usage_widgets::sequential(110.0, 110.0);
    let busy_cells = painted_rects(&screen)
        .into_iter()
        .filter(|rect| rect.fill == busiest && rect.rect.size() == egui::vec2(14.0, 14.0))
        .count();
    assert_eq!(busy_cells, 2);
    for weekday in ["Mon", "Sun"] {
        screen.get_by_label(weekday);
    }

    for share in ["66.7%", "20.0%", "13.3%"] {
        screen.get_by_label(share);
    }
    assert!(top(&screen, "opus") < top(&screen, "gpt"));
    assert!(top(&screen, "gpt") < top(&screen, "haiku"));
    assert_english(&screen);
}

// Given: the Overview by model / When: the distribution is switched to providers
// Then: the models of a provider are summed into one share
#[test]
fn the_distribution_can_split_the_cost_by_provider() {
    let mut screen = answered(None);
    // The distribution sits below the charts; the screen grows so a click reaches it.
    screen.set_size(egui::vec2(800.0, 1400.0));
    steps(&mut screen);
    pick(&mut screen, 1, "Provider");
    screen.get_by_label("anthropic");
    screen.get_by_label("80.0%");
    screen.get_by_label("openai");
    assert!(screen.query_by_label("haiku").is_none());
    assert!(asked(&mut screen).is_empty());
}

fn top(screen: &Harness<'_, Screen>, label: &str) -> f32 {
    screen.get_by_label(label).rect().top()
}

// Given: an answered tab / When: the Breakdown is opened and regrouped
// Then: days come newest first with a Total, weeks and providers group, and Cost sorts in turn
#[test]
fn the_breakdown_groups_by_day_week_and_provider_and_sorts_by_a_column() {
    let mut screen = answered(None);
    screen
        .get_by_role_and_label(Role::Button, "Breakdown")
        .click();
    steps(&mut screen);
    assert!(top(&screen, "2026-10-19") < top(&screen, "2026-10-15"));
    screen.get_by_role_and_label(Role::Label, "Total");
    assert_eq!(screen.query_all_by_label("$1.00").count(), 1);
    assert_eq!(screen.query_all_by_label("$1.50*").count(), 1);
    screen.get_by_label("$1.50*").hover();
    steps(&mut screen);
    screen.get_by_label(crate::panes::usage_widgets::LOWER_BOUND_HINT);

    pick(&mut screen, 1, "Week");
    screen.get_by_label("Week of 2026-10-12");
    screen.get_by_label("Week of 2026-10-19");

    pick(&mut screen, 1, "Provider");
    assert!(screen.query_by_label("    └ opus").is_none());
    screen.get_by_label("Model breakdown").click();
    steps(&mut screen);
    screen.get_by_label("    └ opus");
    screen.get_by_label("    └ haiku");

    for marker in ["Cost ▼", "Cost ▲", "Cost"] {
        let header = screen
            .get_all_by_role(Role::Button)
            .find(|node| {
                node.accesskit_node()
                    .label()
                    .is_some_and(|label| label.starts_with("Cost"))
            })
            .expect("the Cost header");
        header.click();
        steps(&mut screen);
        screen.get_by_role_and_label(Role::Button, marker);
    }
    assert!(asked(&mut screen).is_empty());
    assert_english(&screen);
}

// Given: a read that waits / When: Refresh is clicked, then the new read is refused
// Then: Refresh stays usable while waiting and asks anew, and the refusal is shown
#[test]
fn refresh_can_restart_a_waiting_read_and_a_refusal_is_shown() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let first = asked(&mut screen);
    assert!(
        !screen
            .get_by_label("Refresh")
            .accesskit_node()
            .is_disabled()
    );
    assert_english(&screen);
    screen.get_by_label("Refresh").click();
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert_ne!(
        summary_of(&again[0]).operation_id,
        summary_of(&first[1]).operation_id
    );
    answer(
        &mut screen,
        first[1].tag,
        FeatureReply::Query(Err(FeatureError::new("an earlier ask was lost"))),
    );
    assert!(screen.query_by_label("an earlier ask was lost").is_none());
    answer(
        &mut screen,
        again[0].tag,
        FeatureReply::Query(Err(FeatureError::new("the core is busy"))),
    );
    screen.get_by_label("the core is busy");
    assert!(screen.query_by_label("Loading usage…").is_none());
    assert!(
        !screen
            .get_by_label("Refresh")
            .accesskit_node()
            .is_disabled()
    );
}

// Given: an answered tab / When: a refresh fails in the core
// Then: the failure is shown above the usage it last read, which stays drawn until a new answer
#[test]
fn a_failed_refresh_keeps_the_last_usage_and_says_why() {
    let mut screen = answered(None);
    screen.get_by_label("Refresh").click();
    let again = asked(&mut screen);
    let get = summary_of(&again[0]).clone();
    screen.get_by_label("$1.50*");
    answer(
        &mut screen,
        again[0].tag,
        usage(
            &get,
            OperationState::Failed {
                code: ErrorCode::StorageUnavailable,
            },
            0,
        ),
    );
    screen.get_by_label("The storage could not be read.");
    screen.get_by_label("$1.50*");
    screen.get_by_label("Daily tokens");
    assert_english(&screen);

    screen.get_by_label("Refresh").click();
    let last = asked(&mut screen);
    let get = summary_of(&last[0]).clone();
    answer(
        &mut screen,
        last[0].tag,
        usage(&get, OperationState::Ready, 0),
    );
    assert!(
        screen
            .query_by_label("The storage could not be read.")
            .is_none()
    );
    screen.get_by_label("$1.50");
}

// Given: an answered tab / When: another range is picked and its read fails
// Then: the usage of the last range is not drawn under the new one, while waiting or after
#[test]
fn picking_another_range_drops_the_usage_of_the_last_one() {
    let mut screen = answered(None);
    screen.get_by_label("$1.50*");
    pick(&mut screen, 0, "Today");
    let picked = asked(&mut screen);
    assert_eq!(picked.len(), 1);
    screen.get_by_label("Loading usage…");
    assert!(screen.query_by_label("$1.50*").is_none());
    assert!(screen.query_by_label("2026-10-14 to 2026-10-20").is_none());

    let get = summary_of(&picked[0]).clone();
    answer(
        &mut screen,
        picked[0].tag,
        usage(
            &get,
            OperationState::Failed {
                code: ErrorCode::StorageUnavailable,
            },
            0,
        ),
    );
    screen.get_by_label("The storage could not be read.");
    assert!(screen.query_by_label("$1.50*").is_none());
    assert!(screen.query_by_label("Daily tokens").is_none());
}

// Given: the first pass of the tab / When: the core answers a range without requests
// Then: the legacy empty line is shown instead of tiles and charts
#[test]
fn a_range_without_requests_says_so_in_the_legacy_words() {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    let first = asked(&mut screen);
    let get = summary_of(&first[1]).clone();
    let FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(mut outcome)))) =
        usage(&get, OperationState::Ready, 0)
    else {
        unreachable!()
    };
    let summary = outcome.data.as_mut().unwrap();
    summary.totals = UsageTotals::default();
    summary.days.clear();
    summary.models.clear();
    answer(
        &mut screen,
        first[1].tag,
        FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(
            outcome,
        )))),
    );
    screen.get_by_label("No usage recorded for this period.");
    assert!(screen.query_by_label("Daily tokens").is_none());
    assert!(screen.query_by_label("Cost").is_none());
}

fn keep_usage_at(feature: &mut DiagnosticsFeature, now: Instant) -> Vec<FeatureAction> {
    let capabilities = [Capability::Queries];
    let cache = Cache::default();
    let selection = Selection::default();
    let mut frame = FeatureFrame::default();
    let mut ctx = FeatureContext {
        data: &mut super::super::data::DataInbox::default(),
        feature: FeatureId::Diagnostics,
        view: FeatureView {
            connected: true,
            capabilities: &capabilities,
            cache: &cache,
            selection: &selection,
        },
        frame: &mut frame,
    };
    feature.keep_usage(&mut ctx, now);
    frame.actions
}

// Given: an answered usage read / When: passes run before, at and after the refresh time
// Then: it is read anew at it, and neither asked again nor woken for while that read
//       waits past its time
#[test]
fn the_open_tab_reads_its_range_again_every_thirty_seconds() {
    let mut feature = DiagnosticsFeature::default();
    let t0 = Instant::now();
    let first = keep_usage_at(&mut feature, t0);
    assert_eq!(first.len(), 1);
    assert_eq!(feature.usage_refresh_in(t0), None);
    let get = summary_of(&first[0]).clone();
    feature.reply(first[0].tag, usage(&get, OperationState::Ready, 0));
    assert_eq!(feature.usage_refresh_in(t0), Some(USAGE_REFRESH));

    let before = t0 + USAGE_REFRESH - Duration::from_millis(1);
    assert!(keep_usage_at(&mut feature, before).is_empty());
    let refreshed = keep_usage_at(&mut feature, t0 + USAGE_REFRESH);
    assert_eq!(refreshed.len(), 1);
    assert_ne!(summary_of(&refreshed[0]).operation_id, get.operation_id);
    let late = t0 + USAGE_REFRESH * 3;
    assert!(keep_usage_at(&mut feature, late).is_empty());
    assert_eq!(feature.usage_refresh_in(late), None);
}

fn add_run(screen: &mut Harness<'_, Screen>, id: &str, parent: Option<&str>, role: &str) {
    let mut run = serde_json::json!({"id": id, "thread": "t1", "role": role,
        "phase": "running", "revision": "1"});
    if let Some(parent) = parent {
        run["parent_run"] = serde_json::json!(parent);
    }
    let run = serde_json::from_value(run).unwrap();
    screen.state_mut().cache.runs.insert(id.into(), run);
}

fn context_reply(
    get: &GetRunContext,
    messages: Vec<ContextMessage>,
    next: Option<&str>,
) -> FeatureReply {
    let context = RunContext {
        overview: RunContextOverview {
            run: get.run.clone(),
            role: "orchestrator".into(),
            category: None,
            phase: "after_tool_round".into(),
            selected_model: Some("opus".into()),
            updated_at_ms: 0,
            tools: vec!["read".into(), "write".into()],
            tools_omitted: 0,
            message_count: 3,
            compaction_checkpoints: 1,
            segments: vec![
                ContextSegment {
                    label: "user".into(),
                    tokens: 120,
                },
                ContextSegment {
                    label: "system".into(),
                    tokens: 900,
                },
            ],
        },
        messages: Page {
            items: messages,
            next_cursor: next.map(|cursor| PageCursor::new(cursor).unwrap()),
            snapshot_revision: Revision(1),
        },
    };
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Context(
        Box::new(ReadOutcome {
            operation: view(&get.operation_id, OperationState::Ready),
            data: Some(context),
        }),
    ))))
}

fn message(index: u32, role: ContextMessageRole, summary: &str, tokens: u64) -> ContextMessage {
    ContextMessage {
        index,
        role,
        summary: summary.into(),
        tokens,
        listed: true,
    }
}

// Given: a root and a child run of the selected thread
// When: the Context view opens, pages its messages and picks the child
// Then: the root is read first and drawn as the legacy run view, and the child is read on pick
#[test]
fn the_context_view_reads_the_newest_root_first_and_a_picked_run_after() {
    let mut screen = answered(Some("t1"));
    add_run(&mut screen, "run-1", None, "orchestrator");
    add_run(&mut screen, "run-2", Some("run-1"), "worker");
    screen
        .get_by_role_and_label(Role::Button, "Context")
        .click();
    let asked_first = asked(&mut screen);
    assert_eq!(asked_first.len(), 1);
    let get = context_of(&asked_first[0]).clone();
    assert_eq!(get.run.as_str(), "run-1");

    answer(
        &mut screen,
        asked_first[0].tag,
        context_reply(
            &get,
            vec![message(1, ContextMessageRole::User, "fix the build", 120)],
            Some("next"),
        ),
    );
    screen.get_by_label("Messages (1 of 3)");
    assert_eq!(combo_values(&screen), ["run-1 · orchestrator"]);
    screen.get_by_label(
        "orchestrator · opus · 3 messages · 1 compaction checkpoints · saved at after_tool_round",
    );
    screen.get_by_label("Saved history 1.0K");
    screen.get_by_label("system 900");
    assert!(
        screen.query_by_label("Free 0").is_none(),
        "no free share is claimed without a known window"
    );
    screen.get_by_label("user · 120 · fix the build");
    screen.get_by_label("Tools (2)").click();
    steps(&mut screen);
    screen.get_by_label("read");
    screen.get_by_label("write");
    assert_english(&screen);

    screen.get_by_label(context::SHOW_MORE).click();
    let more = asked(&mut screen);
    assert_eq!(more.len(), 1);
    assert!(
        screen.query_by_label(context::SHOW_MORE).is_none(),
        "a page that was asked is not asked twice"
    );
    assert_eq!(
        context_of(&more[0])
            .page
            .cursor
            .as_ref()
            .map(PageCursor::as_str),
        Some("next")
    );
    answer(
        &mut screen,
        more[0].tag,
        context_reply(
            &get,
            vec![
                ContextMessage {
                    listed: false,
                    ..message(2, ContextMessageRole::Assistant, "reasoning", 10)
                },
                message(3, ContextMessageRole::Assistant, "done", 40),
            ],
            None,
        ),
    );
    screen.get_by_label("user · 120 · fix the build");
    screen.get_by_label("assistant · 40 · done");
    assert!(
        screen
            .query_by_label("assistant · 10 · reasoning")
            .is_none()
    );
    screen.get_by_label("Messages (2 of 3)");
    screen.get_by_label("1 replay-only message hidden · counted in the budget");
    assert!(screen.query_by_label(context::SHOW_MORE).is_none());

    pick(&mut screen, 0, "run-2 · worker");
    let picked = asked(&mut screen);
    assert_eq!(picked.len(), 1);
    assert_eq!(context_of(&picked[0]).run.as_str(), "run-2");
}

// Given: legacy sequential and time-ordered root runs of the selected thread
// When: the Context view opens / Then: the newest root is read first and the picker lists by age
#[test]
fn the_context_view_orders_sequential_and_time_ordered_runs_by_age() {
    let mut screen = answered(Some("t1"));
    add_run(&mut screen, "run-9", None, "orchestrator");
    add_run(&mut screen, "run-10", None, "orchestrator");
    add_run(
        &mut screen,
        "run-01k0000000000000000000000a",
        None,
        "orchestrator",
    );
    screen
        .get_by_role_and_label(Role::Button, "Context")
        .click();
    let asked_first = asked(&mut screen);
    assert_eq!(
        context_of(&asked_first[0]).run.as_str(),
        "run-01k0000000000000000000000a"
    );
    let choices: Vec<String> =
        context::run_choices(&screen.state().cache, &ThreadId::new("t1").unwrap())
            .into_iter()
            .map(|choice| choice.run.as_str().to_owned())
            .collect();
    assert_eq!(
        choices,
        ["run-01k0000000000000000000000a", "run-10", "run-9"]
    );
}

// Given: a run's first page was read and the core since evicted its operation
// When: the next page is asked and answered Pending without data
// Then: the run is read afresh from its first page instead of the page being lost
#[test]
fn a_page_of_an_evicted_context_operation_reads_the_run_afresh() {
    let mut screen = answered(Some("t1"));
    add_run(&mut screen, "run-1", None, "orchestrator");
    screen
        .get_by_role_and_label(Role::Button, "Context")
        .click();
    let asked_first = asked(&mut screen);
    let get = context_of(&asked_first[0]).clone();
    answer(
        &mut screen,
        asked_first[0].tag,
        context_reply(
            &get,
            vec![message(1, ContextMessageRole::User, "fix the build", 120)],
            Some("next"),
        ),
    );
    screen.get_by_label(context::SHOW_MORE).click();
    let more = asked(&mut screen);
    assert_eq!(more.len(), 1);
    answer(
        &mut screen,
        more[0].tag,
        FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Context(
            Box::new(ReadOutcome {
                operation: view(&get.operation_id, OperationState::Pending),
                data: None,
            }),
        )))),
    );
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    let fresh = context_of(&again[0]);
    assert_eq!(fresh.run.as_str(), "run-1");
    assert_ne!(fresh.operation_id, get.operation_id);
    assert_eq!(fresh.page.cursor, None);
}

// Given: no thread, then a thread without runs, then a run without a checkpoint
// When: the Context view is drawn / Then: each says so in the legacy words
#[test]
fn the_context_view_says_when_there_is_no_thread_run_or_checkpoint() {
    let mut screen = answered(None);
    screen
        .get_by_role_and_label(Role::Button, "Context")
        .click();
    steps(&mut screen);
    screen.get_by_label(context::NO_THREAD);

    screen.state_mut().selection.thread = Some(ThreadId::new("t1").unwrap());
    steps(&mut screen);
    screen.get_by_label(context::NO_RUNS);

    add_run(&mut screen, "run-1", None, "orchestrator");
    let asked_now = asked(&mut screen);
    let get = context_of(&asked_now[0]).clone();
    answer(
        &mut screen,
        asked_now[0].tag,
        FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Context(
            Box::new(ReadOutcome {
                operation: view(
                    &get.operation_id,
                    OperationState::Failed {
                        code: ErrorCode::NotFound,
                    },
                ),
                data: None,
            }),
        )))),
    );
    screen.get_by_label(context::NO_CHECKPOINT);
    assert_english(&screen);
}

// Given: a run's context was asked / When: a new snapshot resets the feature
// Then: the same run is asked again, and the answer to the earlier ask is dropped
#[test]
fn a_reset_asks_the_chosen_run_again_and_drops_an_earlier_answer() {
    let mut screen = answered(Some("t1"));
    add_run(&mut screen, "run-1", None, "orchestrator");
    screen
        .get_by_role_and_label(Role::Button, "Context")
        .click();
    let before = asked(&mut screen);
    let old = context_of(&before[0]).clone();

    screen.state_mut().feature.reset();
    let after = asked(&mut screen);
    let contexts: Vec<_> = after
        .iter()
        .filter(|action| {
            matches!(
                query_of(action),
                Query::Diagnostics(DiagnosticsQuery::GetRunContext(_))
            )
        })
        .collect();
    assert_eq!(contexts.len(), 1);
    assert_eq!(context_of(contexts[0]).run.as_str(), "run-1");
    assert_ne!(context_of(contexts[0]).operation_id, old.operation_id);

    answer(
        &mut screen,
        before[0].tag,
        context_reply(&old, vec![], None),
    );
    assert!(screen.query_by_label("Tools (2)").is_none());
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
