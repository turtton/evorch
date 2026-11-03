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
    BodyError, BodyErrorKind, BodyResolver, BodyText, DeferredEvent, EventFailure, EventReread,
    FailureKind, HistoryRow, HistorySource, ResolverConfig,
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
    })
}

fn failure(id: i64, run: Option<&str>) -> HistoryRow {
    HistoryRow::Failure(EventFailure {
        id,
        run_id: run.map(str::to_owned),
        kind: FailureKind::Unreadable,
        reason: "the payload does not decode".into(),
    })
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
    assert_eq!(state.body_resolver().unwrap().stats().immediate_reads, 0);
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

#[test]
fn reading_an_event_again_replaces_its_gap_and_replays_nothing_else() {
    let backend = FakeBackend::new();
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
                message(2, "first"),
                failure(3, Some("run-1")),
                message(4, "last"),
            ]),
        )
        .unwrap();
    let notifications = state.notifications.items().count();
    backend.script_event(
        3,
        EventReread::Ready(DeferredEvent {
            id: 3,
            event: Event::new(MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: "recovered".into(),
            }),
            body: None,
        }),
    );

    resolver.request_event(3);
    resolver.pump_all();
    state.bodies_begin_frame(&egui::Context::default());

    assert_eq!(
        thread_entries(&state),
        ["message:first", "message:recovered", "message:last"]
    );
    assert!(state.unreadable_history_events.is_empty());
    assert_eq!(state.notifications.items().count(), notifications);
}
