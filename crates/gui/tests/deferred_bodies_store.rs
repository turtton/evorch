//! Lazy bodies against the real store: a log the writer tokenized, restored
//! without reading its long text, shown by reading it on the worker, and what
//! the pane says when the store cannot give a body back.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::body::{
    BodyBackend, BodyError, BodyErrorKind, BodyRef, BodyResolver, BodyResult, HistoryRow,
    ImmediateMeter, ManualClock, ResolverConfig, StorageBodies,
};
use gui::fixture::DemoSource;
use gui::model::transcript::TranscriptEntry;
use gui::panes::agent::transcript_body;
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

const RUN: &str = "run-1";

struct Log {
    dir: tempfile::TempDir,
    config: StorageConfig,
    answer: String,
    reasoning: String,
    output: String,
    shell: String,
    note: String,
}

fn long(label: &str, bytes: usize) -> String {
    let mut text = String::new();
    let mut n = 0;
    while text.len() < bytes {
        text.push_str(&format!("{label} line {n}. "));
        n += 1;
    }
    text
}

fn event(kind: impl Into<EventKind>) -> Event {
    Event::new(kind)
}

/// A log the writer tokenized: every text over 2 KiB is a blob.
fn tokenized_log() -> Log {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let log = Log {
        answer: long("answer", 6_000),
        reasoning: long("reasoning", 7_000),
        output: long("tool output", 9_000),
        shell: long("shell result", 3_000),
        note: long("note", 4_000),
        dir,
        config,
    };
    let storage = Storage::open(log.config.clone()).unwrap();
    let events = [
        event(LifecycleEvent::AgentRunStarted {
            run_id: RUN.into(),
            parent_run_id: None,
            agent_name: "chat:Worker:t1".into(),
            role: "Worker".into(),
        }),
        event(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: log.answer.clone(),
        }),
        event(MessageEvent::ReasoningDelta {
            run_id: Some(RUN.into()),
            delta: log.reasoning.clone(),
        }),
        event(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "A short closing line.".into(),
        }),
        event(ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            input: Some(serde_json::json!({"path": "src/lib.rs"})),
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }),
        event(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            output: Some(log.output.clone()),
            detail: Some(serde_json::json!({"note": log.note})),
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }),
        event(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-2".into(),
            input: Some(serde_json::json!({"command": "ls"})),
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }),
        event(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "call-2".into(),
            is_error: false,
            output: Some(log.shell.clone()),
            detail: None,
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }),
        event(LifecycleEvent::AgentRunStateChanged {
            run_id: RUN.into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
        }),
    ];
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    storage.close();
    let raw = rusqlite::Connection::open(&log.config.db_path).unwrap();
    let tokenized: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM events WHERE body_format = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tokenized, 4,
        "the long texts were tokenized, the short ones were not"
    );
    log
}

fn sidebar(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("t1"), project, "t1")
        .unwrap();
    sidebar.threads[0].run_ids.push(RUN.into());
    sidebar.switch_thread(&ThreadId::new("t1")).unwrap();
    sidebar
}

fn restore(log: &Log, resolver: &BodyResolver) -> WorkbenchState<DemoSource> {
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar(log.dir.path()))
        .with_body_resolver(resolver.clone());
    state
        .restore_history(&Database::open(&log.config).unwrap())
        .unwrap();
    state
}

struct Pane {
    state: WorkbenchState<DemoSource>,
    resolver: BodyResolver,
    copied: Vec<String>,
}

fn pane(state: WorkbenchState<DemoSource>, resolver: &BodyResolver) -> Harness<'static, Pane> {
    Harness::builder()
        .with_size(egui::vec2(760.0, 1600.0))
        .build_ui_state(
            |ui, pane: &mut Pane| {
                gui::theme::install(ui.ctx());
                gui::body::ui::install(ui.ctx(), pane.resolver.clone());
                transcript_body(ui, pane.state.transcript());
                for command in ui.ctx().output(|output| output.commands.clone()) {
                    if let egui::OutputCommand::CopyText(text) = command {
                        pane.copied.push(text);
                    }
                }
                pane.resolver.finish_frame();
            },
            Pane {
                state,
                resolver: resolver.clone(),
                copied: Vec::new(),
            },
        )
}

fn settle(harness: &mut Harness<'_, Pane>) {
    harness.run_steps(8);
}

fn manual(path: &std::path::Path) -> (BodyResolver, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let resolver = BodyResolver::manual(
        Arc::new(StorageBodies::new(path.to_owned())),
        ResolverConfig {
            clock: clock.clone(),
            ..ResolverConfig::default()
        },
    );
    (resolver, clock)
}

fn thread_entries(state: &WorkbenchState<DemoSource>) -> &[TranscriptEntry] {
    state.transcript().entries()
}

fn blob_of(text: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).to_vec()
}

#[test]
fn restoring_a_tokenized_log_leaves_long_text_in_the_store_and_shows_it_through_the_worker() {
    let log = tokenized_log();
    let backend = Arc::new(StorageBodies::new(log.config.db_path.clone()));
    let (wake, woken) = std::sync::mpsc::channel();
    let resolver = BodyResolver::spawn(
        backend,
        ResolverConfig::default(),
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );

    let state = restore(&log, &resolver);

    // What the restore left: the display bodies as references of the declared size…
    let entries = thread_entries(&state);
    let message = entries
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Message { text, .. } if text.has_deferred() => Some(text),
            _ => None,
        })
        .expect("the long answer is still in the store");
    assert_eq!(message.byte_len(), log.answer.len() as u64);
    assert!(entries.iter().any(|entry| {
        matches!(entry, TranscriptEntry::Reasoning { text, .. }
            if text.has_deferred() && text.byte_len() == log.reasoning.len() as u64)
    }));
    // …the short text as it is, a nested detail and a shell result read at once.
    assert!(entries.iter().any(|entry| {
        matches!(entry, TranscriptEntry::Message { text, .. } if text == "A short closing line.")
    }));
    let read_card = entries
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Tool {
                tool_name,
                output,
                detail,
                ..
            } if tool_name == "read" => Some((output, detail)),
            _ => None,
        })
        .expect("the read card");
    assert!(
        read_card
            .0
            .as_ref()
            .is_some_and(|output| output.has_deferred())
    );
    assert_eq!(read_card.1.as_ref().unwrap()["note"], log.note.as_str());
    assert!(entries.iter().any(|entry| {
        matches!(entry, TranscriptEntry::Tool { tool_name, output: Some(output), .. }
            if tool_name == "shell" && *output == log.shell.as_str())
    }));
    assert_eq!(resolver.stats().bodies_read, 0, "no display body was read");

    // The pane reads on the worker and shows the text.
    let mut harness = pane(state, &resolver);
    settle(&mut harness);
    woken.recv().expect("the worker read the answer");
    settle(&mut harness);
    assert!(
        harness
            .query_by_label_contains("answer line 0. answer line 1.")
            .is_some()
    );
    assert!(harness.query_by_label_contains("Loading content").is_none());
    assert!(
        harness.query_by_label_contains("reasoning line").is_none(),
        "closed, so not read"
    );
    assert_eq!(resolver.stats().bodies_read, 1);
    let readers = resolver.stats().reader_threads;
    assert_eq!(readers.len(), 1);
    assert_ne!(readers[0], std::thread::current().id());

    // Opening the reasoning reads it.
    harness.get_by_label("thinking").click();
    settle(&mut harness);
    woken.recv().expect("the worker read the reasoning");
    settle(&mut harness);
    assert!(
        harness
            .query_by_label_contains("reasoning line 0.")
            .is_some()
    );
    assert_eq!(resolver.stats().bodies_read, 2);
}

#[test]
fn a_missing_blob_is_an_integrity_error_with_details_that_point_at_it() {
    let log = tokenized_log();
    let raw = rusqlite::Connection::open(&log.config.db_path).unwrap();
    // A reference to a blob that is gone is exactly the damage being simulated.
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    let removed = raw
        .execute(
            "DELETE FROM content_blobs WHERE hash = ?1",
            [blob_of(&log.answer)],
        )
        .unwrap();
    assert_eq!(removed, 1);
    drop(raw);
    let (resolver, clock) = manual(&log.config.db_path);
    let state = restore(&log, &resolver);

    let mut harness = pane(state, &resolver);
    settle(&mut harness);
    resolver.pump_all();
    settle(&mut harness);

    assert!(
        harness
            .query_by_label_contains("Couldn't load content")
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("The stored content is missing")
            .is_some()
    );
    assert!(harness.query_by_label("Retry").is_some());
    clock.advance(Duration::from_secs(3_600));
    resolver.pump_all();
    assert_eq!(
        resolver.stats().storage_calls,
        1,
        "nothing is retried by itself"
    );
    harness.get_by_label("Copy details").click();
    settle(&mut harness);
    let copied = harness.state().copied.last().cloned().expect("copied");
    assert!(copied.contains(&format!("byte_len: {}", log.answer.len())));
    assert!(copied.contains("run: run-1"));
    assert!(copied.contains("evorch inspect blob"));
    assert!(copied.contains("missing"));
    assert!(
        !copied.contains("answer line"),
        "the text is not in the details"
    );
    assert!(!copied.contains('\u{e000}'));
    assert_eq!(resolver.take_failure_reports().len(), 1);
}

#[test]
fn a_blob_that_no_longer_matches_is_reported_as_damaged_and_recovers_after_repair() {
    let log = tokenized_log();
    let hash = blob_of(&log.answer);
    let raw = rusqlite::Connection::open(&log.config.db_path).unwrap();
    let (data, codec): (Vec<u8>, i64) = raw
        .query_row(
            "SELECT data, codec FROM content_blobs WHERE hash = ?1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    raw.execute(
        "UPDATE content_blobs SET data = x'0102030405' WHERE hash = ?1",
        [&hash],
    )
    .unwrap();
    let (resolver, _) = manual(&log.config.db_path);
    let state = restore(&log, &resolver);
    let mut harness = pane(state, &resolver);
    settle(&mut harness);
    resolver.pump_all();
    settle(&mut harness);
    assert!(
        harness
            .query_by_label_contains("The stored content is damaged")
            .is_some()
    );

    // Put the blob back (a repair) and retry by hand.
    raw.execute(
        "UPDATE content_blobs SET data = ?2, codec = ?3 WHERE hash = ?1",
        rusqlite::params![hash, data, codec],
    )
    .unwrap();
    harness.get_by_label("Retry").click();
    settle(&mut harness);
    resolver.pump_all();
    settle(&mut harness);
    assert!(harness.query_by_label_contains("answer line 0.").is_some());
    assert!(
        harness
            .query_by_label_contains("Couldn't load content")
            .is_none()
    );
}

/// The real store answering "busy" a given number of times, as a locked
/// database would, before it serves the bodies.
struct Busy {
    inner: StorageBodies,
    left: Mutex<usize>,
}

impl BodyBackend for Busy {
    fn resolve(&self, refs: &[BodyRef]) -> Result<Vec<BodyResult>, BodyError> {
        let mut left = self.left.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err(BodyError::new(BodyErrorKind::Busy, "database is locked"));
        }
        drop(left);
        self.inner.resolve(refs)
    }

    fn read_rows(
        &self,
        runs: &[String],
        after: i64,
        through: i64,
        meter: &ImmediateMeter,
    ) -> Result<Vec<HistoryRow>, BodyError> {
        self.inner.read_rows(runs, after, through, meter)
    }
}

#[test]
fn a_busy_store_is_retried_by_itself_and_then_the_real_text_appears() {
    let log = tokenized_log();
    let clock = Arc::new(ManualClock::new());
    let resolver = BodyResolver::manual(
        Arc::new(Busy {
            inner: StorageBodies::new(log.config.db_path.clone()),
            left: Mutex::new(2),
        }),
        ResolverConfig {
            clock: clock.clone(),
            ..ResolverConfig::default()
        },
    );
    let state = restore(&log, &resolver);
    let mut harness = pane(state, &resolver);
    settle(&mut harness);
    resolver.pump_all();
    settle(&mut harness);
    assert!(
        harness
            .query_by_label_contains("retrying in 1s (retry 1 of 3)")
            .is_some()
    );

    clock.advance(Duration::from_millis(250));
    resolver.pump_all();
    settle(&mut harness);
    assert!(
        harness
            .query_by_label_contains("retrying in 1s (retry 2 of 3)")
            .is_some()
    );

    clock.advance(Duration::from_secs(1));
    resolver.pump_all();
    settle(&mut harness);
    assert!(harness.query_by_label_contains("answer line 0.").is_some());
}

#[test]
fn the_ordinary_readers_still_give_whole_events_while_the_gui_defers_them() {
    // The same log through storage's ordinary reader: whole text, no token.
    let log = tokenized_log();
    let db = Database::open(&log.config).unwrap();
    let all = db.events_all_ordered().unwrap();
    let texts: Vec<_> = all
        .iter()
        .filter_map(|stored| match &stored.event.kind {
            EventKind::Message(MessageEvent::MessageCompleted { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts[0], log.answer);
    assert!(texts.iter().all(|text| !text.starts_with('\u{e000}')));
}

#[test]
fn an_event_whose_nested_text_cannot_be_read_stands_as_a_gap_that_says_its_content_is_unavailable()
{
    let log = tokenized_log();
    let raw = rusqlite::Connection::open(&log.config.db_path).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    // The note nested in the read call's detail is read during the replay.
    raw.execute(
        "DELETE FROM content_blobs WHERE hash = ?1",
        [blob_of(&log.note)],
    )
    .unwrap();
    drop(raw);
    let (resolver, _) = manual(&log.config.db_path);
    let state = restore(&log, &resolver);

    let gaps: Vec<_> = thread_entries(&state)
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Unreadable { kind, run_id, .. } => {
                Some((kind.clone(), run_id.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        gaps,
        [(
            gui::body::FailureKind::Body(BodyErrorKind::Missing),
            Some(RUN.to_owned())
        )]
    );
    let mut harness = pane(state, &resolver);
    settle(&mut harness);
    assert!(
        harness
            .query_by_label_contains("its content is unavailable")
            .is_some()
    );
    assert!(harness.query_by_label("Retry").is_some());
    // The events around it are still there.
    assert!(
        harness
            .query_by_label_contains("A short closing line.")
            .is_some()
    );
}

fn note_reference(log: &Log) -> BodyRef {
    use sha2::{Digest, Sha256};
    BodyRef {
        hash: Sha256::digest(log.note.as_bytes()).into(),
        byte_len: log.note.len() as u64,
    }
}

#[test]
fn the_gap_of_a_missing_nested_text_names_the_body_and_a_repaired_store_fills_it_on_retry() {
    let log = tokenized_log();
    let raw = rusqlite::Connection::open(&log.config.db_path).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    raw.execute(
        "CREATE TABLE saved_blobs AS SELECT * FROM content_blobs WHERE hash = ?1",
        [blob_of(&log.note)],
    )
    .unwrap();
    raw.execute(
        "DELETE FROM content_blobs WHERE hash = ?1",
        [blob_of(&log.note)],
    )
    .unwrap();
    let (resolver, _) = manual(&log.config.db_path);
    let state = restore(&log, &resolver);
    let mut harness = pane(state, &resolver);
    settle(&mut harness);

    // The gap says which body is gone, and its details name it for inspection.
    let Some(TranscriptEntry::Unreadable { reference, .. }) =
        thread_entries(&harness.state().state)
            .iter()
            .find(|entry| matches!(entry, TranscriptEntry::Unreadable { .. }))
    else {
        panic!("a gap")
    };
    let reference = *reference;
    let wanted = note_reference(&log);
    assert_eq!(reference, Some(wanted));
    harness.get_by_label("Copy details").click();
    settle(&mut harness);
    let copied = harness.state().copied.last().cloned().expect("details");
    assert!(
        copied.contains(&format!("hash: {}", wanted.hash_hex())),
        "{copied}"
    );
    assert!(copied.contains(&format!("byte_len: {}", wanted.byte_len)));
    assert!(!copied.contains("note line"), "never the text");

    // The store is repaired; Retry reads the run again on the worker.
    raw.execute_batch("INSERT INTO content_blobs SELECT * FROM saved_blobs")
        .unwrap();
    harness.get_by_label("Retry").click();
    settle(&mut harness);
    harness.state_mut().state.settle_body_rows();
    settle(&mut harness);

    assert!(
        harness
            .query_by_label_contains("its content is unavailable")
            .is_none(),
        "the gap is gone"
    );
    let entries = thread_entries(&harness.state().state);
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::Unreadable { .. }))
    );
    let call = entries.iter().find_map(|entry| match entry {
        TranscriptEntry::Tool {
            call_id, output, ..
        } if call_id == "call-1" => Some(output.is_some()),
        _ => None,
    });
    assert_eq!(call, Some(true), "the call shows its result now");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Tool { call_id, .. } if call_id == "call-1"))
            .count(),
        1,
        "the call's card was completed in place, not shown twice"
    );
}

#[test]
fn what_a_restore_reads_at_once_is_counted_by_what_needed_it() {
    let log = tokenized_log();
    let (resolver, _) = manual(&log.config.db_path);

    let state = restore(&log, &resolver);

    let stats = resolver.stats();
    // The note nested in a call's detail, and the result of a shell call that a
    // job card parses: two bodies, each in its own class.
    assert_eq!(
        stats.immediate.nested,
        gui::body::ClassStats {
            bodies: 1,
            bytes: log.note.len() as u64
        }
    );
    assert_eq!(
        stats.immediate.control_result,
        gui::body::ClassStats {
            bodies: 1,
            bytes: log.shell.len() as u64
        }
    );
    assert_eq!(
        stats.immediate.feed_display,
        gui::body::ClassStats::default()
    );
    assert_eq!(
        stats.immediate.part_hand_over,
        gui::body::ClassStats::default()
    );
    // Held outside the cache for as long as the event is built: the larger of the two.
    assert_eq!(
        stats.immediate.largest_event_bytes,
        log.note.len().max(log.shell.len()) as u64
    );
    // The display bodies (answer, reasoning, tool output) were not read at all.
    assert_eq!(stats.bodies_read, 0);
    assert_eq!(stats.storage_calls, 0);
    drop(state);
}
