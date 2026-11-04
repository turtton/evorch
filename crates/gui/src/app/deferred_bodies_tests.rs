//! How a restore and the app treat bodies that stay in the store and events
//! that cannot be read: where the gaps stand, what is announced, and what a
//! deleted thread leaves behind.

use std::sync::Arc;

use event_bus::{Event, LifecycleEvent, MessageEvent};
use storage::Database;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

use super::WorkbenchState;
use crate::body::testing::FakeBackend;
use crate::body::{
    BodyError, BodyErrorKind, BodyResolver, BodyText, DeferredEvent, EventFailure, EventPhase,
    FailureKind, HistoryRow, HistorySource, ManualClock, ResolverConfig,
};
use crate::fixture::DemoSource;
use crate::model::transcript::TranscriptEntry;

/// A log made of the rows a test lists, in that order.
struct Rows(Vec<HistoryRow>);

impl HistorySource for Rows {
    fn scan(
        &self,
        _through_id: i64,
        visit: &mut dyn FnMut(HistoryRow),
    ) -> Result<(), storage::StorageError> {
        self.0.iter().cloned().for_each(visit);
        Ok(())
    }
}

fn event(id: i64, kind: impl Into<event_bus::EventKind>) -> HistoryRow {
    HistoryRow::Event(DeferredEvent {
        id,
        event: Event::new(kind),
        body: None,
        first_id: None,
    })
}

fn deferred_message(id: i64, body: BodyText) -> HistoryRow {
    HistoryRow::Event(DeferredEvent {
        id,
        event: Event::new(MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: String::new(),
        }),
        body: Some(body),
        first_id: None,
    })
}

fn failure(id: i64, run: Option<&str>) -> HistoryRow {
    HistoryRow::Failure(EventFailure {
        id,
        run_id: run.map(str::to_owned),
        kind: FailureKind::Unreadable,
        reason: "the payload does not decode".into(),
        reference: None,
        wall_clock: None,
    })
}

/// When the events of the tests happened: seconds after a fixed start.
fn at(seconds: u64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + seconds)
}

/// `row` as it was stored `seconds` after the start.
fn happened(mut row: HistoryRow, seconds: u64) -> HistoryRow {
    match &mut row {
        HistoryRow::Event(event) => event.event.meta.wall_clock = at(seconds),
        HistoryRow::Failure(failure) => failure.wall_clock = Some(at(seconds)),
    }
    row
}

fn started() -> HistoryRow {
    event(
        1,
        LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:t1".into(),
            role: "Worker".into(),
        },
    )
}

fn message(id: i64, text: &str) -> HistoryRow {
    event(
        id,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: text.into(),
        },
    )
}

fn sidebar() -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", std::path::Path::new("/tmp"))
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("t1"), project, "t1")
        .unwrap();
    sidebar
        .threads
        .last_mut()
        .unwrap()
        .run_ids
        .push("run-1".into());
    sidebar.switch_thread(&ThreadId::new("t1")).unwrap();
    sidebar
}

fn restored(rows: Vec<HistoryRow>) -> (WorkbenchState<DemoSource>, Arc<FakeBackend>) {
    let backend = FakeBackend::new();
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar())
        .with_body_resolver(BodyResolver::manual(
            backend.clone(),
            ResolverConfig::default(),
        ));
    state
        .restore_history_from(&Database::open_in_memory().unwrap(), &Rows(rows))
        .unwrap();
    (state, backend)
}

fn thread_entries(state: &WorkbenchState<DemoSource>) -> Vec<String> {
    state
        .transcripts
        .thread_model("t1")
        .expect("the thread's transcript")
        .entries()
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => format!("message:{}", text.inline_or_empty()),
            TranscriptEntry::Unreadable { event_id, .. } => format!("gap:{event_id}"),
            other => format!("{other:?}").chars().take(12).collect(),
        })
        .filter(|entry| entry.starts_with("message:") || entry.starts_with("gap:"))
        .collect()
}

#[test]
fn an_unreadable_event_leaves_a_gap_exactly_where_it_was_and_only_one() {
    let (state, _) = restored(vec![
        started(),
        message(2, "first"),
        failure(3, Some("run-1")),
        // The same row is reported by both passes of a restore.
        failure(3, Some("run-1")),
        message(4, "second"),
    ]);

    assert_eq!(
        thread_entries(&state),
        ["message:first", "gap:3", "message:second"]
    );
    let run = state
        .transcripts
        .run("run-1")
        .expect("the run's transcript");
    let gaps = run
        .entries()
        .iter()
        .filter(|entry| matches!(entry, TranscriptEntry::Unreadable { .. }))
        .count();
    assert_eq!(gaps, 1);
    assert_eq!(
        state
            .unreadable_history_events
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [3]
    );
}

#[test]
fn an_unreadable_event_with_no_run_is_never_put_in_the_open_thread_but_is_announced() {
    let (state, _) = restored(vec![
        started(),
        message(2, "first"),
        failure(7, None),
        failure(9, None),
        message(10, "second"),
    ]);

    assert_eq!(thread_entries(&state), ["message:first", "message:second"]);
    let announced: Vec<_> = state
        .notifications
        .items()
        .filter(|item| {
            matches!(
                item.kind,
                crate::model::notifications::NotificationKind::HistoryGap
            )
        })
        .collect();
    assert_eq!(announced.len(), 1);
    assert!(announced[0].summary.contains("2 stored event(s)"));
    assert!(announced[0].summary.contains("7, 9"));
}

#[test]
fn an_unreadable_event_ends_the_response_a_run_was_streaming() {
    let part = |id: i64, index: usize| {
        event(
            id,
            MessageEvent::MessagePart {
                run_id: "run-1".into(),
                response_id: "response".into(),
                text: format!("slice {index} "),
                index,
                total: 3,
            },
        )
    };
    let (state, _) = restored(vec![
        started(),
        part(2, 0),
        failure(3, Some("run-1")),
        part(4, 2),
    ]);

    // Neither half becomes a message, and no completion is invented.
    assert_eq!(thread_entries(&state), ["gap:3"]);
}

#[test]
fn replaying_a_log_opens_no_display_body_and_shows_the_declared_size() {
    let backend_text = "a long answer ".repeat(400);
    let backend = FakeBackend::new();
    let body = BodyText::deferred(backend.deferred(&backend_text, "run-1", 2));
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar())
        .with_body_resolver(BodyResolver::manual(
            backend.clone(),
            ResolverConfig::default(),
        ));
    state
        .restore_history_from(
            &Database::open_in_memory().unwrap(),
            &Rows(vec![started(), deferred_message(2, body.clone())]),
        )
        .unwrap();

    let model = state.transcripts.thread_model("t1").unwrap();
    let TranscriptEntry::Message { text, .. } = &model.entries()[model.entries().len() - 1] else {
        panic!("the answer")
    };
    assert_eq!(text, &body);
    assert_eq!(text.byte_len(), backend_text.len() as u64);
    assert!(backend.calls().is_empty(), "nothing was read");
    assert_eq!(
        state
            .body_resolver()
            .unwrap()
            .stats()
            .immediate
            .total_bodies(),
        0
    );
}

#[test]
fn deleting_a_thread_drops_its_transcript_and_whatever_was_held_for_it() {
    let backend = FakeBackend::new();
    let text = "answer of a doomed thread ".repeat(100);
    let body = backend.deferred(&text, "run-1", 2);
    let resolver = BodyResolver::manual(backend.clone(), ResolverConfig::default());
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar())
        .with_body_resolver(resolver.clone());
    state
        .restore_history_from(
            &Database::open_in_memory().unwrap(),
            &Rows(vec![
                started(),
                deferred_message(2, BodyText::deferred(body.clone())),
                failure(3, Some("run-1")),
            ]),
        )
        .unwrap();
    // The pane drew it and the worker read it.
    let _ = resolver.view(&body);
    resolver.pump_all();
    assert!(resolver.peek(&body).is_some());
    assert!(resolver.cached_bytes() > 0);

    state = state.with_tombstones(storage::thread_retention::TombstoneSet {
        threads: ["t1".to_owned()].into_iter().collect(),
        ..Default::default()
    });
    state.prune_deleted_threads();

    assert!(state.transcripts.thread_model("t1").is_none());
    assert!(state.transcripts.run("run-1").is_none());
    assert!(resolver.peek(&body).is_none(), "its cached text is gone");
    assert_eq!(resolver.cached_bytes(), 0);
}

#[test]
fn a_permanent_failure_is_reported_once_however_many_frames_show_it() {
    let backend = FakeBackend::new();
    let body = backend.deferred("unreadable", "run-1", 2);
    backend.fail_always(
        body.key,
        BodyError::new(BodyErrorKind::Corrupt, "hash mismatch"),
    );
    let resolver = BodyResolver::manual(backend.clone(), ResolverConfig::default());
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar())
        .with_body_resolver(resolver.clone());
    let ctx = egui::Context::default();

    for _ in 0..4 {
        let _ = resolver.view(&body);
        resolver.finish_frame();
        resolver.pump_all();
        state.bodies_begin_frame(&ctx);
    }

    let reports = state.body_failure_reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].failure.key, body.key);
    assert_eq!(reports[0].failure.origin.run_id.as_deref(), Some("run-1"));
}

fn part(id: i64, index: usize, text: &str) -> HistoryRow {
    event(
        id,
        MessageEvent::MessagePart {
            run_id: "run-1".into(),
            response_id: "response".into(),
            text: text.into(),
            index,
            total: 3,
        },
    )
}

/// A restored thread whose sidebar file holds `messages` the user sent, with a
/// resolver driven by hand and a clock the test moves.
fn restored_with_user_messages(
    rows: Vec<HistoryRow>,
    messages: &[(&str, u64)],
) -> (
    WorkbenchState<DemoSource>,
    Arc<FakeBackend>,
    BodyResolver,
    Arc<ManualClock>,
    tempfile::TempDir,
) {
    use super::history::{DeliveryStatus, HistorySidebar, UserMessage};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let saved = HistorySidebar {
        sidebar: sidebar(),
        user_messages: messages
            .iter()
            .map(|(text, seconds)| UserMessage {
                id: format!("user-{text}"),
                thread_id: "t1".into(),
                text: (*text).into(),
                skills: Vec::new(),
                origin_unverified: false,
                image_count: 0,
                resent_from: None,
                at: at(*seconds),
                status: DeliveryStatus::Delivered,
                starts_run: false,
                resolution: None,
            })
            .collect(),
        composer_drafts: Vec::new(),
    };
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let backend = FakeBackend::new();
    let clock = Arc::new(ManualClock::new());
    let resolver = BodyResolver::manual(
        backend.clone(),
        ResolverConfig {
            clock: clock.clone(),
            ..ResolverConfig::default()
        },
    );
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar())
        .with_sidebar_path(path)
        .with_body_resolver(resolver.clone());
    state
        .restore_history_from(&Database::open_in_memory().unwrap(), &Rows(rows))
        .unwrap();
    (state, backend, resolver, clock, dir)
}

fn thread_entries_with_input(state: &WorkbenchState<DemoSource>) -> Vec<String> {
    state
        .transcripts
        .thread_model("t1")
        .expect("the thread's transcript")
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => {
                Some(format!("message:{}", text.inline_or_empty()))
            }
            TranscriptEntry::UserMessage { text, .. } => Some(format!("you:{text}")),
            TranscriptEntry::Unreadable { event_id, .. } => Some(format!("gap:{event_id}")),
            _ => None,
        })
        .collect()
}

#[test]
fn what_the_user_sent_before_an_unreadable_event_stays_before_its_gap() {
    // The user asked at second 2; the answer (event 3) happened at second 3 and
    // cannot be read; the next answer is at second 4.
    let (state, ..) = restored_with_user_messages(
        vec![
            happened(started(), 0),
            happened(message(2, "first"), 1),
            happened(failure(3, Some("run-1")), 3),
            happened(message(4, "second"), 4),
        ],
        &[("ask", 2)],
    );

    assert_eq!(
        thread_entries_with_input(&state),
        ["message:first", "you:ask", "gap:3", "message:second"],
        "the question comes first, as it does before a readable answer"
    );
}

#[test]
fn a_failure_that_does_not_know_when_it_happened_does_not_move_the_input() {
    let (state, ..) = restored_with_user_messages(
        vec![
            happened(started(), 0),
            happened(message(2, "first"), 1),
            failure(3, Some("run-1")),
            happened(message(4, "second"), 4),
        ],
        &[("ask", 2)],
    );

    assert_eq!(
        thread_entries_with_input(&state),
        ["message:first", "gap:3", "you:ask", "message:second"]
    );
}

#[test]
fn reading_an_event_again_replaces_its_gap_and_replays_nothing_else() {
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![
            started(),
            message(2, "first"),
            failure(3, Some("run-1")),
            message(4, "last"),
        ],
        &[],
    );
    let notifications = state.notifications.items().count();
    let before: Vec<usize> = state
        .transcripts
        .thread_model("t1")
        .unwrap()
        .visible_entry_ids()
        .collect();
    backend.script_rows(Ok(vec![
        started(),
        message(2, "first"),
        message(3, "recovered"),
        message(4, "last"),
    ]));

    resolver.request_gap("run-1", 3);
    state.settle_body_rows();

    assert_eq!(
        thread_entries(&state),
        ["message:first", "message:recovered", "message:last"]
    );
    assert!(state.unreadable_history_events.is_empty());
    assert_eq!(state.notifications.items().count(), notifications);
    let after: Vec<usize> = state
        .transcripts
        .thread_model("t1")
        .unwrap()
        .visible_entry_ids()
        .collect();
    let first = before.len() - 3;
    assert_eq!(
        (after[first], after[after.len() - 1]),
        (before[first], before[before.len() - 1]),
        "the entries around the gap keep their identity, so nothing shown moves"
    );
}

#[test]
fn a_repaired_slice_of_a_response_brings_the_whole_response_back() {
    // Slice 1 of 3 was unreadable: the restore shows no response, only the gap.
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![
            started(),
            message(2, "first"),
            part(3, 0, "AAA "),
            failure(4, Some("run-1")),
            part(5, 2, "CCC "),
            message(6, "last"),
        ],
        &[],
    );
    assert_eq!(
        thread_entries(&state),
        ["message:first", "gap:4", "message:last"]
    );
    // The slice is repaired; the worker reads the run again and joins the slices.
    backend.script_rows(Ok(vec![
        started(),
        message(2, "first"),
        part(3, 0, "AAA "),
        part(4, 1, "BBB "),
        part(5, 2, "CCC "),
        message(6, "last"),
    ]));

    resolver.request_gap("run-1", 4);
    state.settle_body_rows();

    assert_eq!(
        thread_entries(&state),
        ["message:first", "message:AAA BBB CCC ", "message:last"],
        "the response is complete, where the gap was, not just a closed gap"
    );
    assert!(state.unreadable_history_events.is_empty());
}

#[test]
fn an_event_that_fails_again_keeps_its_gap_and_says_why() {
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![started(), failure(3, Some("run-1")), message(4, "last")],
        &[],
    );
    backend.script_rows(Ok(vec![
        started(),
        HistoryRow::Failure(EventFailure {
            id: 3,
            run_id: Some("run-1".into()),
            kind: FailureKind::Body(BodyErrorKind::Missing),
            reason: "no such blob".into(),
            reference: Some(FakeBackend::key_of("gone")),
            wall_clock: None,
        }),
        message(4, "last"),
    ]));

    resolver.request_gap("run-1", 3);
    state.settle_body_rows();

    assert_eq!(thread_entries(&state), ["gap:3", "message:last"]);
    let model = state.transcripts.thread_model("t1").unwrap();
    let Some(TranscriptEntry::Unreadable {
        kind,
        reason,
        reference,
        ..
    }) = model
        .entries()
        .iter()
        .find(|entry| matches!(entry, TranscriptEntry::Unreadable { .. }))
    else {
        panic!("the gap")
    };
    assert_eq!(kind, &FailureKind::Body(BodyErrorKind::Missing));
    assert_eq!(reason, "no such blob");
    assert_eq!(reference, &Some(FakeBackend::key_of("gone")));
    assert_eq!(
        state
            .unreadable_history_events
            .get(&3)
            .map(|e| e.reason.as_str()),
        Some("no such blob")
    );
}

#[test]
fn a_busy_content_of_an_unreadable_event_is_retried_by_itself_until_it_reads() {
    let reference = FakeBackend::key_of("nested input");
    let busy = |id: i64| {
        HistoryRow::Failure(EventFailure {
            id,
            run_id: Some("run-1".into()),
            kind: FailureKind::Body(BodyErrorKind::Busy),
            reason: "database is locked".into(),
            reference: Some(reference),
            wall_clock: None,
        })
    };
    let (mut state, backend, resolver, clock, _dir) = restored_with_user_messages(
        vec![started(), message(2, "first"), busy(3), message(4, "last")],
        &[],
    );
    // The restore itself started the retries: nobody pressed Retry.
    assert_eq!(resolver.event_phase(3), EventPhase::Retrying { retries: 1 });
    backend.script_rows(Ok(vec![
        started(),
        message(2, "first"),
        busy(3),
        message(4, "last"),
    ]));
    backend.script_rows(Ok(vec![
        started(),
        message(2, "first"),
        message(3, "now readable"),
        message(4, "last"),
    ]));

    clock.advance(std::time::Duration::from_millis(250));
    state.settle_body_rows();
    assert_eq!(
        thread_entries(&state),
        ["message:first", "gap:3", "message:last"]
    );
    assert_eq!(resolver.event_phase(3), EventPhase::Retrying { retries: 2 });

    clock.advance(std::time::Duration::from_secs(1));
    state.settle_body_rows();

    assert_eq!(
        thread_entries(&state),
        ["message:first", "message:now readable", "message:last"]
    );
    assert_eq!(resolver.event_phase(3), EventPhase::Idle);
    assert_eq!(resolver.stats().row_auto_retries, 1);
}

#[test]
fn a_history_read_that_a_deleted_thread_outlives_is_dropped_and_nothing_is_recreated() {
    let (mut state, backend, resolver, _clock, _dir) =
        restored_with_user_messages(vec![started(), message(2, "first")], &[]);
    // The thread's transcript goes away, then its history is asked for again
    // and the thread is deleted while the worker reads it.
    state.transcripts.drop_thread("t1");
    assert!(state.transcripts.run("run-1").is_none());
    backend.script_rows(Ok(vec![started(), message(2, "first")]));
    let forgetting = resolver.clone();
    backend.on_read(move || forgetting.forget_run("run-1"));
    state.rehydrate_threads(&[("t1".to_owned(), vec!["run-1".to_owned()])]);
    assert!(
        !state.shared_feed.rehydrating.is_empty(),
        "the feed waits for it"
    );

    state.settle_body_rows();

    assert!(
        state.transcripts.run("run-1").is_none(),
        "the deleted run's transcript was not built again"
    );
    assert!(state.transcripts.thread_model("t1").is_none());
    assert!(
        state.shared_feed.rehydrating.is_empty(),
        "and the feed does not wait for a read nobody will apply"
    );
}

fn delta_row(id: i64, text: &str) -> HistoryRow {
    event(
        id,
        MessageEvent::MessageDelta {
            run_id: Some("run-1".into()),
            delta: text.into(),
        },
    )
}

#[test]
fn a_repaired_slice_of_a_stream_gives_one_message_not_the_halves_and_the_whole() {
    // The restore shows the stream before the unreadable delta and the one after it
    // as two messages with the gap between.
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![
            started(),
            delta_row(2, "A "),
            failure(3, Some("run-1")),
            delta_row(4, "C"),
        ],
        &[],
    );
    assert_eq!(thread_entries(&state), ["message:A ", "gap:3", "message:C"]);
    backend.script_rows(Ok(vec![
        started(),
        delta_row(2, "A "),
        delta_row(3, "B "),
        delta_row(4, "C"),
    ]));

    resolver.request_gap("run-1", 3);
    state.settle_body_rows();

    assert_eq!(
        thread_entries(&state),
        ["message:A B C"],
        "the stream is whole once, not as [A, A B C, C]"
    );
    assert!(state.unreadable_history_events.is_empty());
}

#[test]
fn a_repaired_completion_replaces_what_was_streamed_before_it() {
    let completed = |id: i64, text: &str| message(id, text);
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![
            started(),
            delta_row(2, "partial "),
            failure(3, Some("run-1")),
            message(4, "after"),
        ],
        &[],
    );
    // The completion after the gap ends the stream, so it takes the streamed entry's place.
    assert_eq!(thread_entries(&state), ["message:after", "gap:3"]);
    backend.script_rows(Ok(vec![
        started(),
        delta_row(2, "partial "),
        completed(3, "the full answer"),
        message(4, "after"),
    ]));

    resolver.request_gap("run-1", 3);
    state.settle_body_rows();

    assert_eq!(
        thread_entries(&state),
        ["message:the full answer", "message:after"]
    );
}

#[test]
fn deleting_a_thread_drops_the_unreadable_rows_held_for_its_runs() {
    let (mut state, ..) = restored_with_user_messages(
        vec![started(), message(2, "first"), failure(3, Some("run-1"))],
        &[],
    );
    // A row of the run that the shared feed holds back, and one of another run.
    let held = |id: i64, run: &str| EventFailure {
        id,
        run_id: Some(run.to_owned()),
        kind: FailureKind::Unreadable,
        reason: "damaged".into(),
        reference: None,
        wall_clock: None,
    };
    state.shared_feed.failures.insert(8, held(8, "run-1"));
    state
        .shared_feed
        .failures
        .insert(9, held(9, "run-elsewhere"));
    assert!(state.unreadable_history_events.contains_key(&3));

    state = state.with_tombstones(storage::thread_retention::TombstoneSet {
        threads: ["t1".to_owned()].into_iter().collect(),
        ..Default::default()
    });
    state.prune_deleted_threads();

    assert_eq!(
        state
            .shared_feed
            .failures
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [9],
        "only the other run's row is still waiting to be shown"
    );
    assert!(state.unreadable_history_events.is_empty());
}

fn live_delta(text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some("run-1".into()),
        delta: text.into(),
    })
}

#[test]
fn rows_read_before_a_delta_arrived_do_not_overwrite_it_and_the_run_is_read_again() {
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![
            started(),
            delta_row(2, "A "),
            failure(3, Some("run-1")),
            delta_row(4, "C "),
        ],
        &[],
    );
    assert_eq!(
        thread_entries(&state),
        ["message:A ", "gap:3", "message:C "]
    );
    // The rows are read up to where the history reaches (event 4) and their result waits.
    backend.script_rows(Ok(vec![
        started(),
        delta_row(2, "A "),
        delta_row(3, "B "),
        delta_row(4, "C "),
    ]));
    resolver.request_gap("run-1", 3);
    resolver.pump_all();

    // Meanwhile another window's next delta (event 5) is shown in the stream.
    state.transcripts.set_source(Some((5, 5)));
    state.apply_conversation_event(&live_delta("D"));
    state.transcripts.set_source(None);
    resolver.note_shown_through(5);
    backend.script_rows(Ok(vec![
        started(),
        delta_row(2, "A "),
        delta_row(3, "B "),
        delta_row(4, "C "),
        delta_row(5, "D"),
    ]));

    // The older result is not applied over it: the delta stays.
    state.apply_rows_results();
    assert_eq!(
        thread_entries(&state),
        ["message:A ", "gap:3", "message:C D"],
        "nothing was replaced by the stale rows"
    );
    // The run is read again up to event 5 and then nothing is lost.
    state.settle_body_rows();
    assert_eq!(thread_entries(&state), ["message:A B C D"]);
    assert!(state.unreadable_history_events.is_empty());
    assert_eq!(backend.rows_reads().len(), 2);
    assert_eq!(backend.rows_reads()[1].2, 5);
}

#[test]
fn a_run_that_keeps_changing_while_it_is_read_again_is_given_up_with_the_reason_on_the_gap() {
    let (mut state, backend, resolver, _clock, _dir) = restored_with_user_messages(
        vec![
            started(),
            delta_row(2, "A "),
            failure(3, Some("run-1")),
            delta_row(4, "C "),
        ],
        &[],
    );
    resolver.request_gap("run-1", 3);
    // Every read ends, and a newer delta arrives before its result is applied.
    for newer in 5..=8 {
        backend.script_rows(Ok(vec![
            started(),
            delta_row(2, "A "),
            delta_row(3, "B "),
            delta_row(4, "C "),
        ]));
        resolver.pump_all();
        state.transcripts.set_source(Some((newer, newer)));
        state.apply_conversation_event(&live_delta("+"));
        state.transcripts.set_source(None);
        resolver.note_shown_through(newer);
        state.apply_rows_results();
    }

    assert!(!resolver.has_rows_work(), "it stopped asking");
    assert_eq!(backend.rows_reads().len(), 4, "one read and three more");
    let model = state.transcripts.thread_model("t1").unwrap();
    let Some(TranscriptEntry::Unreadable { reason, .. }) = model
        .entries()
        .iter()
        .find(|entry| matches!(entry, TranscriptEntry::Unreadable { .. }))
    else {
        panic!("the gap is still shown")
    };
    assert!(reason.contains("kept changing"), "{reason}");
}
