//! The resolver's contract, driven by hand: nothing is read until a test
//! pumps it, and time moves only when a test advances the clock.

use std::sync::Arc;
use std::time::Duration;

use super::testing::FakeBackend;
use super::*;

struct Rig {
    backend: Arc<FakeBackend>,
    clock: Arc<ManualClock>,
    resolver: BodyResolver,
}

fn rig_with(configure: impl FnOnce(&mut ResolverConfig)) -> Rig {
    let backend = FakeBackend::new();
    let clock = Arc::new(ManualClock::new());
    let mut config = ResolverConfig {
        clock: clock.clone(),
        ..ResolverConfig::default()
    };
    configure(&mut config);
    let resolver = BodyResolver::manual(backend.clone(), config);
    Rig {
        backend,
        clock,
        resolver,
    }
}

fn rig() -> Rig {
    rig_with(|_| {})
}

impl Rig {
    /// One drawn frame that shows `bodies`, then the worker runs once.
    fn frame(&self, bodies: &[&DeferredBody]) -> Vec<BodyView> {
        let views = bodies.iter().map(|body| self.resolver.view(body)).collect();
        self.resolver.finish_frame();
        views
    }

    fn view(&self, body: &DeferredBody) -> BodyView {
        self.frame(&[body]).remove(0)
    }
}

fn busy() -> BodyError {
    BodyError::new(BodyErrorKind::Busy, "database is locked")
}

fn missing() -> BodyError {
    BodyError::new(BodyErrorKind::Missing, "no such blob")
}

fn ready_text(view: &BodyView) -> &str {
    match view {
        BodyView::Ready(text) => text,
        other => panic!("expected the text, got {other:?}"),
    }
}

#[test]
fn a_body_is_loading_until_the_worker_reads_it_and_then_it_is_text() {
    let rig = rig();
    let body = rig.backend.deferred("the whole answer", "run", 1);

    assert!(matches!(rig.view(&body), BodyView::Loading { bytes: 16 }));
    assert!(rig.backend.calls().is_empty(), "a view never reads");

    assert!(rig.resolver.pump());
    assert_eq!(ready_text(&rig.view(&body)), "the whole answer");
    assert_eq!(rig.backend.calls().len(), 1);
}

#[test]
fn the_same_body_shown_in_many_places_is_read_once() {
    let rig = rig();
    let key = rig.backend.add("shared text");
    let first = DeferredBody::new(key, Some("run-a"), Some(1));
    let second = DeferredBody::new(key, Some("run-b"), Some(9));

    rig.frame(&[&first, &second, &first]);
    rig.frame(&[&first, &second]);
    rig.resolver.pump_all();

    assert_eq!(rig.backend.calls(), vec![vec![key]]);
    let stats = rig.resolver.stats();
    assert_eq!(stats.cache_misses, 1);
    assert!(stats.joined_requests >= 3);
}

#[test]
fn a_busy_store_is_retried_on_the_schedule_and_then_the_text_appears() {
    let rig = rig();
    let body = rig.backend.deferred("eventually", "run", 1);
    rig.backend.fail_first(body.key, &busy(), 3);

    rig.view(&body);
    rig.resolver.pump_all();
    let waiting = rig.view(&body);
    let BodyView::Retrying {
        attempt, retry_in, ..
    } = waiting
    else {
        panic!("expected a scheduled retry, got {waiting:?}")
    };
    assert_eq!((attempt, retry_in), (1, Duration::from_millis(250)));
    // Not before it is due.
    rig.clock.advance(Duration::from_millis(249));
    rig.view(&body);
    assert!(!rig.resolver.pump());

    for (due, next) in [(1, 1_000u64), (2, 4_000)] {
        rig.clock
            .advance(Duration::from_millis(if due == 1 { 1 } else { 1_000 }));
        rig.view(&body);
        assert!(rig.resolver.pump(), "retry {due} runs when due");
        let BodyView::Retrying {
            retry_in, attempt, ..
        } = rig.view(&body)
        else {
            panic!("expected another retry")
        };
        assert_eq!(
            (attempt, retry_in.as_millis()),
            (due + 1, u128::from(next)),
            "after retry {due}"
        );
        if due == 1 {
            rig.clock.advance(Duration::from_millis(999));
        }
    }
    rig.clock.advance(Duration::from_millis(4_000));
    rig.view(&body);
    assert!(rig.resolver.pump());
    assert_eq!(ready_text(&rig.view(&body)), "eventually");
    assert_eq!(rig.backend.bodies_read(), 4, "one read and three retries");
    assert_eq!(rig.resolver.stats().auto_retries, 3);
}

#[test]
fn a_store_that_stays_unavailable_fails_after_three_retries_and_says_so_once() {
    let rig = rig();
    let body = rig.backend.deferred("never arrives", "run-7", 42);
    rig.backend
        .fail_always(body.key, BodyError::new(BodyErrorKind::Io, "disk error"));

    rig.view(&body);
    rig.resolver.pump_all();
    for delay in [250, 1_000, 4_000] {
        rig.clock.advance(Duration::from_millis(delay));
        rig.view(&body);
        rig.resolver.pump_all();
    }
    let BodyView::Failed(failure) = rig.view(&body) else {
        panic!("expected a permanent failure")
    };
    assert_eq!(failure.attempts, 4);
    assert_eq!(failure.error.kind, BodyErrorKind::Io);
    assert_eq!(rig.backend.bodies_read(), 4);

    // It does not try again by itself, however long it waits.
    rig.clock.advance(Duration::from_secs(600));
    rig.view(&body);
    assert!(!rig.resolver.pump());
    assert_eq!(rig.resolver.take_failure_reports().len(), 1);
    // A manual retry that fails the same way is not reported a second time.
    rig.resolver.retry(&body);
    rig.resolver.pump_all();
    assert!(matches!(rig.view(&body), BodyView::Retrying { .. }));
    assert!(rig.resolver.take_failure_reports().is_empty());
}

#[test]
fn missing_and_corrupt_bodies_are_never_retried_by_themselves_and_are_reported_once() {
    for kind in [BodyErrorKind::Missing, BodyErrorKind::Corrupt] {
        let rig = rig();
        let body = rig.backend.deferred("gone", "run", 3);
        rig.backend
            .fail_always(body.key, BodyError::new(kind, "integrity"));

        rig.view(&body);
        rig.resolver.pump_all();
        rig.clock.advance(Duration::from_secs(3_600));
        for _ in 0..3 {
            let view = rig.view(&body);
            assert!(
                matches!(&view, BodyView::Failed(f) if f.error.kind == kind),
                "{view:?}"
            );
            rig.resolver.pump_all();
        }
        assert_eq!(rig.backend.bodies_read(), 1, "{kind:?}");
        let reports = rig.resolver.take_failure_reports();
        assert_eq!(reports.len(), 1, "{kind:?}");
        assert_eq!(reports[0].failure.origin.event_id, Some(3));
    }
}

#[test]
fn a_manual_retry_of_a_failed_body_succeeds_without_a_second_request_in_flight() {
    let rig = rig();
    let body = rig.backend.deferred("recovered", "run", 1);
    rig.backend.fail_first(body.key, &missing(), 1);
    rig.view(&body);
    rig.resolver.pump_all();
    assert!(matches!(rig.view(&body), BodyView::Failed(_)));

    rig.resolver.retry(&body);
    rig.resolver.retry(&body);
    rig.frame(&[&body]);
    rig.resolver.pump_all();

    assert_eq!(ready_text(&rig.view(&body)), "recovered");
    assert_eq!(rig.backend.bodies_read(), 2);
    assert_eq!(rig.resolver.stats().manual_retries, 1);
    // The earlier failure is forgotten: a later failure is reported afresh.
    assert_eq!(rig.resolver.take_failure_reports().len(), 1);
}

#[test]
fn a_read_for_a_body_that_left_the_screen_before_it_started_is_dropped() {
    let rig = rig();
    let gone = rig.backend.deferred("scrolled past", "run", 1);
    let shown = rig.backend.deferred("still in view", "run", 2);

    rig.frame(&[&gone, &shown]);
    rig.frame(&[&shown]);
    rig.resolver.pump_all();

    assert_eq!(rig.backend.calls(), vec![vec![shown.key]]);
    assert_eq!(rig.resolver.stats().cancelled, 1);
    // Scrolling back asks again.
    assert!(matches!(rig.view(&gone), BodyView::Loading { .. }));
    rig.resolver.pump_all();
    assert_eq!(ready_text(&rig.view(&gone)), "scrolled past");
}

#[test]
fn a_result_that_arrives_after_the_run_was_forgotten_is_never_shown() {
    let rig = rig();
    let body = rig.backend.deferred("deleted thread text", "doomed", 1);
    let resolver = rig.resolver.clone();
    rig.backend.on_read(move || resolver.forget_run("doomed"));

    rig.view(&body);
    rig.resolver.pump_all();

    assert_eq!(rig.resolver.stats().late_results_dropped, 1);
    assert_eq!(rig.resolver.cached_bytes(), 0);
    assert!(rig.resolver.peek(&body).is_none(), "nothing was kept");
    rig.backend.on_read(|| {});
    // A later request reads it afresh and nothing stale is served.
    assert!(matches!(rig.view(&body), BodyView::Loading { .. }));
}

#[test]
fn forgetting_a_run_drops_its_cached_text_and_failures_but_not_a_shared_body_s_other_owner() {
    let rig = rig();
    let key = rig.backend.add("shared between two runs");
    let a = DeferredBody::new(key, Some("a"), Some(1));
    let b = DeferredBody::new(key, Some("b"), Some(2));
    let only_a = rig.backend.deferred("only a has this", "a", 3);
    rig.frame(&[&a, &b, &only_a]);
    rig.resolver.pump_all();

    rig.resolver.forget_run("a");

    assert!(rig.resolver.peek(&a).is_some(), "run b still shows it");
    assert!(rig.resolver.peek(&only_a).is_none());
    rig.resolver.forget_run("b");
    assert!(rig.resolver.peek(&b).is_none());
    assert_eq!(rig.resolver.cached_bytes(), 0);
}

#[test]
fn resolved_text_is_kept_within_the_cache_and_evicted_oldest_first() {
    let rig = rig_with(|config| config.cache_bytes = 100);
    let bodies: Vec<_> = (0..3)
        .map(|n| {
            rig.backend
                .deferred(&format!("{n}{}", "x".repeat(39)), "run", n)
        })
        .collect();
    for body in &bodies {
        rig.frame(&[body]);
        rig.resolver.pump_all();
    }
    assert!(rig.resolver.cached_bytes() <= 100);
    assert_eq!(rig.resolver.stats().evictions, 1);

    // The oldest was evicted: showing it again reads it again. The newest hits.
    assert!(rig.resolver.peek(&bodies[2]).is_some());
    let reads = rig.backend.bodies_read();
    assert!(matches!(rig.view(&bodies[0]), BodyView::Loading { .. }));
    rig.resolver.pump_all();
    assert_eq!(rig.backend.bodies_read(), reads + 1);
    assert!(rig.resolver.cached_bytes() <= 100);
}

#[test]
fn a_body_over_the_cache_is_held_only_while_it_is_shown() {
    let rig = rig_with(|config| config.cache_bytes = 50);
    let big = rig.backend.deferred(&"b".repeat(80), "run", 1);
    rig.view(&big);
    rig.resolver.pump_all();

    assert_eq!(ready_text(&rig.view(&big)).len(), 80);
    assert_eq!(rig.resolver.cached_bytes(), 0, "it is not in the cache");
    // A frame that does not show it releases it.
    rig.frame(&[]);
    rig.frame(&[]);
    assert!(rig.resolver.peek(&big).is_none());
}

#[test]
fn batches_hold_at_most_256_bodies_and_16_mib_and_a_larger_body_goes_alone() {
    let rig = rig();
    let many: Vec<_> = (0..300)
        .map(|n| rig.backend.deferred(&format!("body number {n}"), "run", n))
        .collect();
    let refs: Vec<&DeferredBody> = many.iter().collect();
    rig.frame(&refs);
    rig.resolver.pump_all();
    let sizes: Vec<_> = rig.backend.calls().iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![256, 44]);

    // Declared sizes decide the split; nothing is read to find them out.
    let rig = rig_with(|config| config.batch_bytes = 25);
    let sizes = [
        ("a".repeat(12), 1),
        ("b".repeat(12), 2),
        ("c".repeat(30), 3),
        ("d".repeat(5), 4),
    ];
    let bodies: Vec<_> = sizes
        .iter()
        .map(|(text, n)| rig.backend.deferred(text, "run", *n))
        .collect();
    let refs: Vec<&DeferredBody> = bodies.iter().collect();
    rig.frame(&refs);
    rig.resolver.pump_all();
    let calls: Vec<Vec<u64>> = rig
        .backend
        .calls()
        .iter()
        .map(|call| call.iter().map(|key| key.byte_len).collect())
        .collect();
    assert_eq!(calls, vec![vec![12, 12], vec![30], vec![5]]);
    assert_eq!(rig.resolver.stats().largest_batch_bytes, 30);
}

#[test]
fn a_body_over_the_absolute_limit_is_refused_without_asking_the_store() {
    let rig = rig_with(|config| config.max_body_bytes = 50);
    let body = rig.backend.deferred(&"z".repeat(100), "run", 1);
    rig.view(&body);
    rig.resolver.pump_all();

    let BodyView::Failed(failure) = rig.view(&body) else {
        panic!("expected a refusal")
    };
    assert_eq!(failure.error.kind, BodyErrorKind::TooLarge);
    assert!(rig.backend.calls().is_empty());
}

#[test]
fn a_failed_call_for_the_whole_batch_applies_to_every_body_in_it() {
    let rig = rig();
    let one = rig.backend.deferred("one", "run", 1);
    let two = rig.backend.deferred("two", "run", 2);
    rig.backend.fail_next_call(busy());
    rig.frame(&[&one, &two]);
    rig.resolver.pump_all();
    let views = rig.frame(&[&one, &two]);
    assert!(
        views
            .iter()
            .all(|view| matches!(view, BodyView::Retrying { .. }))
    );
    rig.clock.advance(Duration::from_millis(250));
    rig.frame(&[&one, &two]);
    rig.resolver.pump_all();
    assert_eq!(ready_text(&rig.view(&one)), "one");
    assert_eq!(ready_text(&rig.view(&two)), "two");
}

#[test]
fn an_operation_waits_for_every_piece_and_holds_it_while_it_waits() {
    let rig = rig();
    let first = rig.backend.deferred("alpha ", "run", 1);
    let second = rig.backend.deferred("omega", "run", 1);
    let mut text = BodyText::deferred(first.clone());
    text.push_str("and ");
    text.push_body(BodyText::deferred(second));

    let mut pin = rig.resolver.pin(&text);
    assert_eq!(pin.poll(), PinState::Pending { bytes: 6 + 4 + 5 });
    // No frame shows these bodies, yet the operation's wait is not cancelled.
    rig.frame(&[]);
    rig.frame(&[]);
    rig.resolver.pump_all();
    assert_eq!(pin.poll(), PinState::Ready("alpha and omega".to_owned()));

    drop(pin);
    let late = rig.backend.deferred("late", "run", 2);
    let mut other = rig.resolver.pin(&BodyText::deferred(late));
    other.poll();
    drop(other);
    rig.frame(&[]);
    rig.frame(&[]);
    assert_eq!(rig.resolver.stats().cancelled, 1, "released once dropped");
}

#[test]
fn an_operation_on_a_body_that_cannot_be_read_fails_instead_of_yielding_a_token_or_nothing() {
    let rig = rig();
    let body = rig.backend.deferred("unreadable", "run-x", 5);
    rig.backend.fail_always(body.key, missing());
    let mut pin = rig.resolver.pin(&BodyText::deferred(body));
    pin.poll();
    rig.resolver.pump_all();
    let PinState::Failed(failure) = pin.poll() else {
        panic!("expected a failure")
    };
    assert_eq!(failure.error.kind, BodyErrorKind::Missing);
    assert!(failure.details().contains("run-x"));
}

fn failure_row(id: i64, kind: FailureKind, reason: &str) -> HistoryRow {
    HistoryRow::Failure(EventFailure {
        id,
        run_id: Some("run-a".into()),
        kind,
        reason: reason.into(),
        reference: None,
        wall_clock: None,
    })
}

#[test]
fn an_unreadable_event_is_read_again_once_however_often_it_is_asked() {
    let rig = rig();
    rig.resolver.note_shown_through(40);
    rig.backend.script_rows(Ok(vec![failure_row(
        7,
        FailureKind::Unreadable,
        "still damaged",
    )]));
    assert_eq!(rig.resolver.event_phase(7), EventPhase::Idle);
    rig.resolver.request_gap("run-a", 7);
    rig.resolver.request_gap("run-a", 7);
    assert_eq!(rig.resolver.event_phase(7), EventPhase::Loading);

    rig.resolver.pump_all();

    assert_eq!(
        rig.backend.rows_reads(),
        vec![(vec!["run-a".to_owned()], 0, 40)],
        "the run is read up to where the history reaches when the read starts"
    );
    assert_eq!(
        rig.resolver.event_phase(7),
        EventPhase::Failed("still damaged".into())
    );
    assert_eq!(rig.resolver.take_rows_results().len(), 1);
    assert!(rig.resolver.take_rows_results().is_empty());
    // Asking again after a failure reads again.
    rig.resolver.request_gap("run-a", 7);
    rig.resolver.pump_all();
    assert_eq!(rig.backend.rows_reads().len(), 2);
}

#[test]
fn an_event_whose_content_is_busy_is_read_again_by_itself_on_the_schedule() {
    let rig = rig();
    let busy_row = || {
        failure_row(
            7,
            FailureKind::Body(BodyErrorKind::Busy),
            "database is locked",
        )
    };
    for _ in 0..4 {
        rig.backend.script_rows(Ok(vec![busy_row()]));
    }
    rig.resolver.request_gap("run-a", 7);
    rig.resolver.pump_all();

    // Each failed try waits, and nothing is reported while tries are left.
    assert_eq!(
        rig.resolver.event_phase(7),
        EventPhase::Retrying { retries: 1 }
    );
    assert!(rig.resolver.take_rows_results().is_empty());
    // The waits are 250 ms, 1 s and 4 s, none of them cut short.
    for (wait_ms, reads, retries) in [(250, 2, 2_u8), (1_000, 3, 3)] {
        rig.clock.advance(Duration::from_millis(wait_ms - 1));
        rig.resolver.pump_all();
        assert_eq!(
            rig.backend.rows_reads().len(),
            reads - 1,
            "not before {wait_ms} ms"
        );
        rig.clock.advance(Duration::from_millis(1));
        rig.resolver.pump_all();
        assert_eq!(rig.backend.rows_reads().len(), reads);
        assert_eq!(
            rig.resolver.event_phase(7),
            EventPhase::Retrying { retries }
        );
    }
    rig.clock.advance(Duration::from_millis(3_999));
    rig.resolver.pump_all();
    assert_eq!(rig.backend.rows_reads().len(), 3);
    rig.clock.advance(Duration::from_millis(1));
    rig.resolver.pump_all();

    assert_eq!(rig.backend.rows_reads().len(), 4);
    assert_eq!(
        rig.resolver.event_phase(7),
        EventPhase::Failed("database is locked".into())
    );
    assert_eq!(
        rig.resolver.take_rows_results().len(),
        1,
        "reported once, at the end"
    );
    assert_eq!(rig.resolver.stats().row_auto_retries, 3);
}

#[test]
fn an_event_that_becomes_readable_while_retrying_is_reported_as_read() {
    let rig = rig();
    rig.backend.script_rows(Ok(vec![failure_row(
        7,
        FailureKind::Body(BodyErrorKind::Io),
        "i/o",
    )]));
    rig.backend.script_rows(Ok(Vec::new()));
    rig.resolver.request_gap("run-a", 7);
    rig.resolver.pump_all();
    rig.clock.advance(Duration::from_millis(250));
    rig.resolver.pump_all();

    assert_eq!(rig.resolver.event_phase(7), EventPhase::Idle);
    let results = rig.resolver.take_rows_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].result, Ok(Vec::new()));
}

#[test]
fn only_a_failure_that_can_pass_starts_retrying_on_its_own() {
    let rig = rig();
    rig.resolver.watch_gap("run-a", 1, &FailureKind::Unreadable);
    rig.resolver
        .watch_gap("run-a", 2, &FailureKind::Body(BodyErrorKind::Missing));
    rig.resolver
        .watch_gap("run-a", 3, &FailureKind::Body(BodyErrorKind::Corrupt));
    assert!(!rig.resolver.has_rows_work());
    assert!(rig.resolver.is_idle());

    rig.resolver
        .watch_gap("run-a", 4, &FailureKind::Body(BodyErrorKind::Busy));
    // Watching twice is one wait.
    rig.resolver
        .watch_gap("run-a", 4, &FailureKind::Body(BodyErrorKind::Busy));
    assert_eq!(
        rig.resolver.event_phase(4),
        EventPhase::Retrying { retries: 1 }
    );
    assert!(rig.backend.rows_reads().is_empty());
    rig.clock.advance(Duration::from_millis(250));
    rig.resolver.pump_all();
    assert_eq!(rig.backend.rows_reads().len(), 1);
}

#[test]
fn a_read_of_rows_that_fails_busy_is_retried_and_one_that_fails_for_good_is_reported() {
    let rig = rig();
    rig.backend.script_rows(Err(busy()));
    rig.backend.script_rows(Err(missing()));
    rig.resolver.request_rows(
        RowsPurpose::Rehydrate { thread: "t".into() },
        vec!["run-a".into()],
        0,
        9,
    );
    rig.resolver.pump_all();
    assert!(rig.resolver.rehydrating("t"));
    assert!(rig.resolver.take_rows_results().is_empty());

    rig.clock.advance(Duration::from_millis(250));
    rig.resolver.pump_all();

    assert!(!rig.resolver.rehydrating("t"));
    let results = rig.resolver.take_rows_results();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].result.as_ref().unwrap_err().kind,
        BodyErrorKind::Missing
    );
}

#[test]
fn forgetting_a_run_drops_the_rows_reads_waiting_for_it() {
    let rig = rig();
    rig.resolver.request_rows(
        RowsPurpose::Rehydrate { thread: "t".into() },
        vec!["run-a".into()],
        0,
        9,
    );
    rig.resolver.forget_run("run-a");
    rig.resolver.pump_all();
    assert!(rig.backend.rows_reads().is_empty());
    assert!(!rig.resolver.rehydrating("t"));
}

#[test]
fn start_up_reads_join_the_pieces_in_order_and_count_apart_from_frame_reads() {
    let rig = rig();
    let a = rig.backend.deferred("12345", "run", 1);
    let b = rig.backend.deferred("abc", "run", 1);
    let mut text = BodyText::from("<");
    text.push_body(BodyText::deferred(a.clone()));
    text.push_body(BodyText::deferred(b));
    text.push_body(BodyText::deferred(a));

    assert_eq!(rig.resolver.read_now(&text).unwrap(), "<12345abc12345");
    let stats = rig.resolver.stats();
    assert_eq!(
        stats.immediate.part_hand_over,
        ClassStats {
            bodies: 2,
            bytes: 8
        }
    );
    assert_eq!(stats.bodies_read, 0);
}

#[test]
fn the_worker_thread_reads_and_the_caller_never_does() {
    let backend = FakeBackend::new();
    let (wake, woken) = std::sync::mpsc::channel();
    let resolver = BodyResolver::spawn(
        backend.clone(),
        ResolverConfig::default(),
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let body = backend.deferred("read elsewhere", "run", 1);

    assert!(matches!(resolver.view(&body), BodyView::Loading { .. }));
    woken.recv().expect("the worker announces its read");
    assert_eq!(ready_text(&resolver.view(&body)), "read elsewhere");

    let readers = backend.reader_threads();
    assert_eq!(readers.len(), 1);
    assert_ne!(readers[0], std::thread::current().id());
    assert_eq!(resolver.stats().reader_threads, readers);
}

#[test]
fn a_worker_retries_by_the_injected_clock_and_stops_when_the_resolver_is_dropped() {
    let backend = FakeBackend::new();
    let clock = Arc::new(ManualClock::new());
    let (wake, woken) = std::sync::mpsc::channel();
    let resolver = BodyResolver::spawn(
        backend.clone(),
        ResolverConfig {
            clock: clock.clone(),
            ..ResolverConfig::default()
        },
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let body = backend.deferred("after a retry", "run", 1);
    backend.fail_first(body.key, &busy(), 1);

    let _ = resolver.view(&body);
    woken.recv().unwrap();
    resolver.finish_frame();
    assert!(matches!(resolver.view(&body), BodyView::Retrying { .. }));
    assert_eq!(backend.bodies_read(), 1);
    // Dropping the last handle stops the worker and releases the backend.
    let weak = Arc::downgrade(&backend);
    drop(backend);
    drop(resolver);
    assert!(weak.upgrade().is_none(), "the worker released its backend");
}
