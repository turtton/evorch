//! The cost of the selected thread and the Live view, driven the way the dock drives a
//! feature: maintained every frame, rendered only while its view is drawn.
use super::thread_usage::REASK_GAP;
use super::*;
use crate::core_client::cache::Cache;
use crate::core_client::features::execution::tests::{Screen, answer, asked, query_of, screen};
use crate::core_client::features::{FeatureAction, FeatureFrame, FeatureView, Selection};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use workbench_protocol::{
    ids::ThreadId,
    namespaces::diagnostics::{
        GetThreadUsage, LIVE_USAGE_MINUTES, ReadOutcome, UsageChange, UsageValue,
    },
    operation::OperationView,
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

fn thread(id: &str) -> ThreadId {
    ThreadId::new(id).unwrap()
}

/// A cache that knows the threads, and the runs as `(run, thread)`.
fn cache_of(threads: &[&str], runs: &[(&str, &str)]) -> Cache {
    let mut cache = Cache::default();
    for id in threads {
        let summary = serde_json::from_value(json!({
            "id": id, "project": "p", "title": id, "state": "idle",
            "revision": "1", "updated_at": "1",
        }))
        .unwrap();
        cache.threads.insert((*id).into(), summary);
    }
    for (run, thread) in runs {
        let summary = serde_json::from_value(json!({
            "id": run, "thread": thread, "role": "orchestrator",
            "phase": "running", "revision": "1",
        }))
        .unwrap();
        cache.runs.insert((*run).into(), summary);
    }
    cache
}

/// One pass of the feature on `selected`, as `maintain` is called each frame: what it asked.
fn maintain_all(
    feature: &mut DiagnosticsFeature,
    cache: &Cache,
    selected: Option<&str>,
    visibility: Visibility,
    now: Instant,
) -> Vec<FeatureAction> {
    let capabilities = [Capability::Queries];
    let selection = Selection {
        project: None,
        thread: selected.map(thread),
    };
    let mut frame = FeatureFrame::default();
    let mut ctx = FeatureContext {
        data: &mut super::super::data::DataInbox::default(),
        feature: FeatureId::Diagnostics,
        view: FeatureView {
            connected: visibility.available,
            capabilities: &capabilities,
            cache,
            selection: &selection,
        },
        frame: &mut frame,
    };
    feature.keep_today(&mut ctx, visibility, now);
    feature.keep_thread_usage(&mut ctx, visibility, now);
    frame.actions
}

/// [`maintain_all`], keeping the reads of a thread's usage.
fn maintain_with(
    feature: &mut DiagnosticsFeature,
    cache: &Cache,
    selected: Option<&str>,
    visibility: Visibility,
    now: Instant,
) -> Vec<FeatureAction> {
    maintain_all(feature, cache, selected, visibility, now)
        .into_iter()
        .filter(|action| thread_query(action).is_some())
        .collect()
}

fn shown() -> Visibility {
    Visibility {
        available: true,
        expanded: true,
        minimized: false,
    }
}

fn thread_query(action: &FeatureAction) -> Option<&GetThreadUsage> {
    match query_of(action) {
        Query::Diagnostics(DiagnosticsQuery::GetThreadUsage(get)) => Some(get),
        _ => None,
    }
}

fn value(requests: u64, micro_usd: u64) -> UsageValue {
    UsageValue {
        totals: UsageTotals {
            requests,
            recorded_cost_micro_usd: micro_usd,
            ..Default::default()
        },
        incomplete: false,
    }
}

fn thread_reply(
    get: &GetThreadUsage,
    state: OperationState,
    total: u64,
    main: u64,
) -> FeatureReply {
    let data = (state == OperationState::Ready).then(|| ThreadUsage {
        thread: get.thread.clone(),
        total: value(3, total),
        conversation: value(1, main),
    });
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::ThreadUsage(
        Box::new(ReadOutcome {
            operation: view(&get.operation_id, state),
            data,
        }),
    ))))
}

fn changed(runs: &[&str]) -> UiEvent {
    UiEvent::Diagnostics(DiagnosticsEvent::UsageChanged {
        change: UsageChange {
            all_runs: runs.is_empty(),
            runs: runs.iter().map(|run| RunId::new(*run).unwrap()).collect(),
        },
    })
}

fn settled(id: &OperationId) -> UiEvent {
    UiEvent::Diagnostics(DiagnosticsEvent::DiagnosticsOperationChanged {
        view: view(id, OperationState::Ready),
    })
}

fn total_of(feature: &DiagnosticsFeature, id: &str) -> Option<u64> {
    feature
        .thread_usage(&thread(id))
        .map(|usage| usage.total.totals.recorded_cost_micro_usd)
}

const SECOND: Duration = Duration::from_secs(1);

// Given: a selected thread the core knows
// When: the screen passes several times and the core answers
// Then: the thread is asked once, its cost belongs to that thread alone, and nothing is asked again
#[test]
fn the_selected_thread_is_asked_once_and_its_cost_is_shown_for_it_alone() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1", "t2"], &[]);
    let t0 = Instant::now();
    let first = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    assert_eq!(first.len(), 1);
    let get = thread_query(&first[0]).unwrap().clone();
    assert_eq!(get.thread, thread("t1"));
    assert!(maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + SECOND).is_empty());

    feature.reply(
        first[0].tag,
        thread_reply(&get, OperationState::Ready, 2_000_000, 500_000),
    );
    assert_eq!(total_of(&feature, "t1"), Some(2_000_000));
    assert_eq!(total_of(&feature, "t2"), None);
    assert!(maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + 60 * SECOND).is_empty());
}

// Given: a thread read with its settling event seen before its reply, answered, and then
//        a newer read started by a notice
// When: the older read's reply comes after the newer read began
// Then: the event asks the same operation again, and the older reply never replaces a newer read
#[test]
fn an_older_read_never_replaces_a_newer_one() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1"], &[("r1", "t1")]);
    let t0 = Instant::now();
    let first = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    let old = thread_query(&first[0]).unwrap().clone();

    // The operation settles before its Pending reply is seen.
    feature.event(&settled(&old.operation_id));
    let again = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    assert_eq!(again.len(), 1);
    assert_eq!(thread_query(&again[0]), Some(&old));
    feature.reply(
        again[0].tag,
        thread_reply(&old, OperationState::Ready, 1_000_000, 0),
    );
    assert_eq!(total_of(&feature, "t1"), Some(1_000_000));

    // A notice starts a newer read while the first ask of the old one is still unanswered.
    feature.event(&changed(&["r1"]));
    let third = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + 3 * SECOND);
    let new = thread_query(&third[0]).unwrap().clone();
    assert_ne!(new.operation_id, old.operation_id);
    feature.reply(
        first[0].tag,
        thread_reply(&old, OperationState::Ready, 9_000_000, 0),
    );
    assert_eq!(total_of(&feature, "t1"), Some(1_000_000));
    feature.reply(
        third[0].tag,
        thread_reply(&new, OperationState::Ready, 5_000_000, 0),
    );
    assert_eq!(total_of(&feature, "t1"), Some(5_000_000));
}

// Given: a read of t1 in flight / When: the selection moves to t2 and t1's answer comes
// Then: t2 is asked, and t1's answer is dropped instead of being shown under t2
#[test]
fn an_answer_for_a_thread_no_longer_selected_is_dropped() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1", "t2"], &[]);
    let t0 = Instant::now();
    let first = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    let for_t1 = thread_query(&first[0]).unwrap().clone();

    let second = maintain_with(&mut feature, &cache, Some("t2"), shown(), t0);
    assert_eq!(second.len(), 1);
    let for_t2 = thread_query(&second[0]).unwrap().clone();
    assert_eq!(for_t2.thread, thread("t2"));

    feature.reply(
        first[0].tag,
        thread_reply(&for_t1, OperationState::Ready, 9_000_000, 0),
    );
    assert_eq!(total_of(&feature, "t1"), None);
    assert_eq!(total_of(&feature, "t2"), None);
    feature.reply(
        second[0].tag,
        thread_reply(&for_t2, OperationState::Ready, 1_000_000, 0),
    );
    assert_eq!(total_of(&feature, "t2"), Some(1_000_000));
    assert_eq!(total_of(&feature, "t1"), None);

    // Back to t1: its earlier cost is not kept; it is asked anew.
    let back = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    assert_eq!(thread_query(&back[0]).unwrap().thread, thread("t1"));
}

// Given: a read in flight / When: notices arrive while it waits and it settles
// Then: nothing is asked meanwhile, and one read follows the settled one, never two
#[test]
fn notices_during_a_read_are_asked_once_after_it_settled() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1"], &[]);
    let t0 = Instant::now();
    let first = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    let get = thread_query(&first[0]).unwrap().clone();

    feature.event(&changed(&[]));
    feature.event(&changed(&[]));
    feature.event(&changed(&["unknown-run"]));
    assert!(
        maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + 10 * SECOND).is_empty(),
        "the read still waits"
    );

    feature.event(&settled(&get.operation_id));
    feature.reply(
        first[0].tag,
        thread_reply(&get, OperationState::Pending, 0, 0),
    );
    let again = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + 10 * SECOND);
    assert_eq!(again.len(), 1);
    assert_eq!(
        thread_query(&again[0]),
        Some(&get),
        "the settled operation is read"
    );
    feature.reply(
        again[0].tag,
        thread_reply(&get, OperationState::Ready, 1_000_000, 0),
    );

    let followed = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + 10 * SECOND);
    assert_eq!(followed.len(), 1, "the notices are asked once");
    let next = thread_query(&followed[0]).unwrap().clone();
    assert_ne!(next.operation_id, get.operation_id);
    feature.reply(
        followed[0].tag,
        thread_reply(&next, OperationState::Ready, 2_000_000, 0),
    );
    assert!(maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + 40 * SECOND).is_empty());
    assert_eq!(feature.thread_usage_wake_in(t0 + 40 * SECOND), None);
    assert_eq!(total_of(&feature, "t1"), Some(2_000_000));
}

// Given: a notice right after a read / When: the gap has not passed
// Then: the thread waits for it, the screen is told when to wake, and then it is asked
#[test]
fn a_busy_run_does_not_set_the_pace_of_the_thread_reads() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1"], &[]);
    let t0 = Instant::now();
    let first = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    let get = thread_query(&first[0]).unwrap().clone();
    feature.reply(
        first[0].tag,
        thread_reply(&get, OperationState::Ready, 1, 1),
    );

    feature.event(&changed(&[]));
    let soon = t0 + SECOND;
    assert!(maintain_with(&mut feature, &cache, Some("t1"), shown(), soon).is_empty());
    assert_eq!(feature.thread_usage_wake_in(soon), Some(REASK_GAP - SECOND));
    let due = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0 + REASK_GAP);
    assert_eq!(due.len(), 1);
}

// Given: notices naming runs / When: the cache says whose they are
// Then: only a run of the selected thread, or one the cache does not know, asks again
#[test]
fn a_notice_asks_again_only_when_it_may_concern_the_selected_thread() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1", "t2"], &[("mine", "t1"), ("other", "t2")]);
    let t0 = Instant::now();
    let first = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    let get = thread_query(&first[0]).unwrap().clone();
    feature.reply(
        first[0].tag,
        thread_reply(&get, OperationState::Ready, 1, 1),
    );

    let later = t0 + 30 * SECOND;
    feature.event(&changed(&["other"]));
    assert!(maintain_with(&mut feature, &cache, Some("t1"), shown(), later).is_empty());

    feature.event(&changed(&["mine"]));
    assert_eq!(
        maintain_with(&mut feature, &cache, Some("t1"), shown(), later).len(),
        1
    );
}

// Given: a minimized window or a lost connection / When: notices arrive and passes run
// Then: nothing is asked; the cost is asked as soon as the window is shown again
#[test]
fn nothing_is_asked_while_the_window_is_minimized_or_unavailable() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1"], &[]);
    let t0 = Instant::now();
    let minimized = Visibility {
        minimized: true,
        ..shown()
    };
    let unavailable = Visibility {
        available: false,
        ..shown()
    };
    assert!(maintain_with(&mut feature, &cache, Some("t1"), minimized, t0).is_empty());
    assert!(maintain_with(&mut feature, &cache, Some("t1"), unavailable, t0).is_empty());
    feature.event(&changed(&[]));
    assert!(maintain_with(&mut feature, &cache, Some("t1"), minimized, t0).is_empty());
    assert_eq!(
        maintain_with(&mut feature, &cache, Some("t1"), shown(), t0).len(),
        1
    );
}

// Given: a new conversation the core does not know / When: passes run
// Then: nothing is asked for it, so no read of it fails
#[test]
fn a_thread_the_core_has_not_seen_is_not_asked() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&[], &[]);
    let t0 = Instant::now();
    assert!(maintain_with(&mut feature, &cache, Some("new"), shown(), t0).is_empty());
    assert!(maintain_with(&mut feature, &cache, None, shown(), t0).is_empty());
}

// ---- Live ----------------------------------------------------------------------

fn live_fixture() -> LiveUsage {
    let day_start_ms = 1_770_681_600_000;
    let spent = |requests: u64, micro: u64| UsageTotals {
        requests,
        input_tokens: 1_000 * requests,
        recorded_cost_micro_usd: micro,
        ..Default::default()
    };
    let mut minutes = vec![UsageTotals::default(); LIVE_USAGE_MINUTES];
    minutes[LIVE_USAGE_MINUTES - 1] = spent(2, 2_000_000);
    minutes[LIVE_USAGE_MINUTES - 20] = spent(1, 1_000_000);
    LiveUsage {
        as_of_ms: day_start_ms + 12 * 3_600_000,
        minutes,
        local_day: "2026-02-10".into(),
        day_start_ms,
        day_end_ms: day_start_ms + 24 * 3_600_000,
        today: spent(3, 3_000_000),
        month: spent(3, 30_000_000),
        quota_windows: Vec::new(),
    }
}

fn live_query(action: &FeatureAction) -> Option<&GetLiveUsage> {
    match query_of(action) {
        Query::Diagnostics(DiagnosticsQuery::GetLiveUsage(get)) => Some(get),
        _ => None,
    }
}

fn live_reply(get: &GetLiveUsage, state: OperationState) -> FeatureReply {
    let data = (state == OperationState::Ready).then(live_fixture);
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::LiveUsage(
        Box::new(ReadOutcome {
            operation: view(&get.operation_id, state),
            data,
        }),
    ))))
}

fn keep_live_at(feature: &mut DiagnosticsFeature, now: Instant) -> Vec<FeatureAction> {
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
    feature.keep_live(&mut ctx, now);
    frame.actions
}

fn live_screen() -> Harness<'static, Screen> {
    let mut screen = screen(Box::<DiagnosticsFeature>::default(), None);
    // Nothing but today's cost is asked while the Live view is not drawn.
    let before = asked(&mut screen);
    assert!(before.iter().all(|action| live_query(action).is_none()));
    screen.get_by_label("Live").click();
    screen
}

// Given: a Live view that is not drawn / When: passes run and notices arrive
// Then: the live usage is never asked
#[test]
fn the_live_usage_is_not_asked_while_its_view_is_not_drawn() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1"], &[]);
    let t0 = Instant::now();
    for step in 0..5 {
        feature.event(&changed(&[]));
        let asked = maintain_all(
            &mut feature,
            &cache,
            Some("t1"),
            shown(),
            t0 + step * 10 * SECOND,
        );
        assert!(asked.iter().all(|action| live_query(action).is_none()));
    }
}

// Given: the Live view drawn / When: a notice arrives while the read waits, and it settles
// Then: one read follows after the gap, and the open view reads again every refresh period
#[test]
fn a_notice_during_the_live_read_is_asked_once_after_it_settled() {
    for notice in [
        changed(&["any"]),
        UiEvent::Diagnostics(DiagnosticsEvent::ProviderQuotasChanged {}),
    ] {
        let mut feature = DiagnosticsFeature::default();
        let t0 = Instant::now();
        let first = keep_live_at(&mut feature, t0);
        assert_eq!(first.len(), 1);
        let get = live_query(&first[0]).unwrap().clone();

        feature.event(&notice);
        assert!(
            keep_live_at(&mut feature, t0 + 10 * SECOND).is_empty(),
            "the read waits"
        );
        feature.event(&settled(&get.operation_id));
        feature.reply(first[0].tag, live_reply(&get, OperationState::Pending));
        let again = keep_live_at(&mut feature, t0 + 10 * SECOND);
        assert_eq!(live_query(&again[0]), Some(&get));
        feature.reply(again[0].tag, live_reply(&get, OperationState::Ready));
        assert!(feature.live.is_some());

        let followed = keep_live_at(&mut feature, t0 + 10 * SECOND);
        assert_eq!(
            followed.len(),
            1,
            "the notice is asked once the read settled"
        );
        let next = live_query(&followed[0]).unwrap().clone();
        assert_ne!(next.operation_id, get.operation_id);
        feature.reply(followed[0].tag, live_reply(&next, OperationState::Ready));
        assert!(keep_live_at(&mut feature, t0 + 11 * SECOND).is_empty());
        assert_eq!(
            keep_live_at(&mut feature, t0 + 10 * SECOND + LIVE_REFRESH).len(),
            1,
            "the open view reads again by itself"
        );
    }
}

// Given: a notice right after a read / When: the gap has not passed
// Then: the live usage waits for it, and the screen is told when to wake
#[test]
fn a_busy_run_does_not_set_the_pace_of_the_live_reads() {
    let mut feature = DiagnosticsFeature::default();
    let t0 = Instant::now();
    let first = keep_live_at(&mut feature, t0);
    let get = live_query(&first[0]).unwrap().clone();
    feature.reply(first[0].tag, live_reply(&get, OperationState::Ready));
    feature.event(&changed(&[]));
    assert!(keep_live_at(&mut feature, t0 + SECOND).is_empty());
    assert_eq!(
        feature.live_refresh_in(t0 + SECOND),
        Some(LIVE_REASK_GAP - SECOND)
    );
    assert_eq!(keep_live_at(&mut feature, t0 + LIVE_REASK_GAP).len(), 1);
}

// Given: the Usage tab / When: Live is opened
// Then: its read is asked, it draws the pace and the forecasts, and there is no Refresh to press
#[test]
fn the_live_view_draws_the_recent_pace_and_forecasts_without_a_refresh_button() {
    let mut screen = live_screen();
    let actions = asked(&mut screen);
    let [action] = actions.as_slice() else {
        panic!("one live read was expected, got {}", actions.len());
    };
    let get = live_query(action).unwrap().clone();
    answer(
        &mut screen,
        action.tag,
        live_reply(&get, OperationState::Ready),
    );

    for label in [
        "Burn rate",
        "$3.00/h",
        "≈ $39.00 by midnight at this pace",
        "$30.00",
        "Requests per minute",
        "Tokens per minute",
    ] {
        let _ = screen
            .get_all_by_label(label)
            .next()
            .unwrap_or_else(|| panic!("{label}"));
    }
    assert!(screen.query_by_label("Refresh").is_none());
    assert!(screen.query_by_label("Subscription windows").is_none());

    screen.get_by_label("15m").click();
    screen.run_steps(2);
    screen.get_by_label("$8.00/h");
}

// Given: unpriced tokens inside the window and in today and the month
// When: the Live view is drawn
// Then: the pace, the window, today and the month are marked as lower bounds
#[test]
fn the_live_view_marks_lower_bounds_where_the_ledger_lacks_a_price() {
    let mut screen = live_screen();
    let actions = asked(&mut screen);
    let get = live_query(&actions[0]).unwrap().clone();
    let mut live = live_fixture();
    live.minutes[LIVE_USAGE_MINUTES - 1].unpriced_tokens = 7;
    live.today.unpriced_tokens = 7;
    live.month.unpriced_tokens = 7;
    let reply = FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::LiveUsage(
        Box::new(ReadOutcome {
            operation: view(&get.operation_id, OperationState::Ready),
            data: Some(live),
        }),
    ))));
    answer(&mut screen, actions[0].tag, reply);
    for label in ["$3.00/h*", "$30.00*"] {
        let _ = screen
            .get_all_by_label(label)
            .next()
            .unwrap_or_else(|| panic!("{label}"));
    }
    assert!(screen.get_all_by_label("$3.00*").count() >= 2);
}

// Given: a read of the thread and of Live in flight / When: the session is reset and the
//        old answers arrive
// Then: they are dropped, and both are read again by themselves
#[test]
fn a_reset_during_a_read_drops_the_old_answer_and_reads_again() {
    let mut feature = DiagnosticsFeature::default();
    let cache = cache_of(&["t1"], &[]);
    let t0 = Instant::now();
    let thread_ask = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    let old_thread = thread_query(&thread_ask[0]).unwrap().clone();
    let live_ask = keep_live_at(&mut feature, t0);
    let old_live = live_query(&live_ask[0]).unwrap().clone();

    feature.reset();
    feature.reply(
        thread_ask[0].tag,
        thread_reply(&old_thread, OperationState::Ready, 9_000_000, 0),
    );
    feature.reply(
        live_ask[0].tag,
        live_reply(&old_live, OperationState::Ready),
    );
    assert_eq!(total_of(&feature, "t1"), None);
    assert!(feature.live.is_none());

    let thread_again = maintain_with(&mut feature, &cache, Some("t1"), shown(), t0);
    assert_eq!(thread_again.len(), 1);
    let new_thread = thread_query(&thread_again[0]).unwrap().clone();
    assert_ne!(new_thread.operation_id, old_thread.operation_id);
    let live_again = keep_live_at(&mut feature, t0);
    assert_eq!(live_again.len(), 1);
    let new_live = live_query(&live_again[0]).unwrap().clone();
    assert_ne!(new_live.operation_id, old_live.operation_id);

    feature.reply(
        thread_again[0].tag,
        thread_reply(&new_thread, OperationState::Ready, 1_000_000, 0),
    );
    feature.reply(
        live_again[0].tag,
        live_reply(&new_live, OperationState::Ready),
    );
    assert_eq!(total_of(&feature, "t1"), Some(1_000_000));
    assert!(feature.live.is_some());
}
