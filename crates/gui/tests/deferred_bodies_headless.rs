//! What a pane shows for a body that is still in the store, state by state.
//!
//! The store is a fake that answers only when a test pumps the resolver, and
//! time is a clock a test advances, so every state is reached on purpose and
//! none depends on how fast anything runs.

use std::sync::Arc;
use std::time::Duration;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{CompactionEvent, CompactionReason, Event, MessageEvent, ToolEvent};
use gui::body::testing::FakeBackend;
use gui::body::{
    BodyError, BodyErrorKind, BodyResolver, BodyText, DeferredEvent, EventReread, ManualClock,
    ResolverConfig,
};
use gui::model::transcript::TranscriptEntry;
use gui::model::transcript_registry::TranscriptRegistry;
use gui::panes::agent::transcript_body;

const RUN: &str = "run-1";

struct State {
    registry: TranscriptRegistry,
    resolver: BodyResolver,
    copied: Vec<String>,
    /// Draw only the rows in view, as the real pane does without accessibility.
    virtualize: bool,
}

struct Rig {
    harness: Harness<'static, State>,
    backend: Arc<FakeBackend>,
    clock: Arc<ManualClock>,
}

fn registry() -> TranscriptRegistry {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("thread", RUN);
    registry.select_thread(Some("thread".into()));
    registry
}

fn rig_with(registry: impl FnOnce(&FakeBackend, &mut TranscriptRegistry)) -> Rig {
    rig_sized(egui::vec2(720.0, 1400.0), registry)
}

fn rig_sized(
    size: egui::Vec2,
    registry: impl FnOnce(&FakeBackend, &mut TranscriptRegistry),
) -> Rig {
    let backend = FakeBackend::new();
    let clock = Arc::new(ManualClock::new());
    let resolver = BodyResolver::manual(
        backend.clone(),
        ResolverConfig {
            clock: clock.clone(),
            ..ResolverConfig::default()
        },
    );
    let mut registry_value = self::registry();
    registry(&backend, &mut registry_value);
    let harness = Harness::builder().with_size(size).build_ui_state(
        |ui, state: &mut State| {
            gui::theme::install(ui.ctx());
            gui::body::ui::install(ui.ctx(), state.resolver.clone());
            if state.virtualize {
                gui::panes::agent::set_transcript_virtualization_for_tests(ui.ctx(), true);
            }
            transcript_body(ui, state.registry.thread());
            for command in ui.ctx().output(|output| output.commands.clone()) {
                if let egui::OutputCommand::CopyText(text) = command {
                    state.copied.push(text);
                }
            }
            state.resolver.finish_frame();
        },
        State {
            registry: registry_value,
            resolver,
            copied: Vec::new(),
            virtualize: false,
        },
    );
    Rig {
        harness,
        backend,
        clock,
    }
}

fn completed(registry: &mut TranscriptRegistry, body: BodyText) {
    registry.apply_with_body(
        &Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: String::new(),
        }),
        Some(&body),
    );
}

fn text_of(words: usize) -> String {
    "word ".repeat(words)
}

impl Rig {
    fn run(&mut self) {
        self.harness.run_steps(8);
    }

    /// The worker reads what is queued, then the pane draws.
    fn read(&mut self) {
        self.harness.state().resolver.pump_all();
        self.harness.run_steps(8);
    }

    fn resolver(&self) -> BodyResolver {
        self.harness.state().resolver.clone()
    }

    fn has(&self, text: &str) -> bool {
        self.harness.query_by_label_contains(text).is_some()
    }

    fn top(&self, text: &str) -> f32 {
        self.harness.get_by_label_contains(text).rect().top()
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.run();
    }
}

fn busy() -> BodyError {
    BodyError::new(BodyErrorKind::Busy, "database is locked")
}

#[test]
fn a_body_in_the_store_shows_how_big_it_is_until_the_worker_has_read_it() {
    let mut rig = rig_with(|backend, registry| {
        completed(
            registry,
            BodyText::deferred(backend.deferred(&text_of(1_000), RUN, 5)),
        );
    });

    rig.run();
    assert!(rig.has("Loading content… (about 4.9 KB)"));
    assert!(!rig.has("word word"));
    assert!(rig.backend.calls().is_empty(), "drawing reads nothing");

    rig.read();
    assert!(rig.has("word word"));
    assert!(!rig.has("Loading content"));
    assert_eq!(rig.backend.calls().len(), 1);
}

#[test]
fn what_is_around_a_body_does_not_move_while_it_loads_and_comes_back_to_the_same_place() {
    // Smaller than the text, so the pane would follow the end once it is read.
    let mut rig = rig_sized(egui::vec2(720.0, 480.0), |backend, registry| {
        registry.push_thread(TranscriptEntry::Notice {
            text: "Before the answer".into(),
        });
        completed(
            registry,
            BodyText::deferred(backend.deferred(&text_of(900), RUN, 5)),
        );
        registry.push_thread(TranscriptEntry::Notice {
            text: "After the answer".into(),
        });
    });
    rig.run();
    let before = rig.top("Before the answer");

    rig.read();
    assert_eq!(
        rig.top("Before the answer"),
        before,
        "the entry above stays put"
    );
    let after_when_read = rig.top("After the answer");

    // The cache lets go of it (as it would for an old body): the placeholder
    // takes the room the text took, so what follows does not jump.
    rig.resolver().forget_run(RUN);
    rig.run();
    assert!(rig.has("Loading content"));
    assert_eq!(rig.top("Before the answer"), before);
    assert!(
        (rig.top("After the answer") - after_when_read).abs() < 1.0,
        "the next entry moved from {after_when_read} to {}",
        rig.top("After the answer")
    );
}

#[test]
fn a_busy_store_shows_a_scheduled_retry_each_time_and_then_the_text() {
    let mut rig = rig_with(|backend, registry| {
        let body = backend.deferred(&text_of(600), RUN, 5);
        backend.fail_first(body.key, &busy(), 3);
        completed(registry, BodyText::deferred(body));
    });
    rig.run();
    rig.read();

    for (delay, text) in [
        (250, "retrying in 1s (retry 1 of 3)"),
        (1_000, "retrying in 1s (retry 2 of 3)"),
        (4_000, "retrying in 4s (retry 3 of 3)"),
    ] {
        // The line says so while the retry waits, and nothing reads before it is due.
        assert!(rig.has(text) || rig.has("retry"), "waiting for {delay} ms");
        let reads = rig.backend.bodies_read();
        rig.clock.advance(Duration::from_millis(delay - 1));
        rig.read();
        assert_eq!(rig.backend.bodies_read(), reads, "not due yet");
        rig.clock.advance(Duration::from_millis(1));
        rig.read();
        assert_eq!(rig.backend.bodies_read(), reads + 1, "due after {delay} ms");
    }
    assert!(rig.has("word word"));
    assert!(!rig.has("Couldn't load content"));
}

#[test]
fn the_retry_line_names_the_wait_and_the_attempt() {
    let mut rig = rig_with(|backend, registry| {
        let body = backend.deferred(&text_of(600), RUN, 5);
        backend.fail_always(body.key, busy());
        completed(registry, BodyText::deferred(body));
    });
    rig.run();
    rig.read();
    assert!(rig.has("Couldn't load content, retrying in 1s (retry 1 of 3)"));
    assert!(!rig.has("Retry"), "no button while it retries by itself");
    rig.clock.advance(Duration::from_millis(250));
    rig.read();
    assert!(rig.has("retrying in 1s (retry 2 of 3)"));
    rig.clock.advance(Duration::from_millis(1_000));
    rig.read();
    assert!(rig.has("retrying in 4s (retry 3 of 3)"));
}

#[test]
fn when_the_retries_run_out_the_failure_stays_in_place_with_retry_and_copy_details() {
    let mut rig = rig_with(|backend, registry| {
        registry.push_thread(TranscriptEntry::Notice {
            text: "Before the answer".into(),
        });
        let body = backend.deferred(&text_of(600), RUN, 5);
        backend.fail_always(body.key, busy());
        completed(registry, BodyText::deferred(body));
        registry.push_thread(TranscriptEntry::Notice {
            text: "After the answer".into(),
        });
    });
    rig.run();
    let before = rig.top("Before the answer");
    rig.read();
    for delay in [250, 1_000, 4_000] {
        rig.clock.advance(Duration::from_millis(delay));
        rig.read();
    }

    assert!(rig.has("Couldn't load content"));
    assert!(rig.has("The database stayed unavailable after 3 retries"));
    assert!(rig.harness.query_by_label("Retry").is_some());
    assert!(rig.harness.query_by_label("Copy details").is_some());
    assert_eq!(rig.backend.bodies_read(), 4);
    assert_eq!(rig.top("Before the answer"), before);
    assert!(rig.top("After the answer") > rig.top("Couldn't load content"));
    // Nothing more is tried by itself.
    rig.clock.advance(Duration::from_secs(3_600));
    rig.read();
    assert_eq!(rig.backend.bodies_read(), 4);
}

#[test]
fn a_missing_body_is_an_integrity_error_at_once_and_the_copied_details_never_hold_the_text() {
    let secret = "hunter2 ".repeat(300);
    let mut rig = rig_with(|backend, registry| {
        let body = backend.deferred(&secret, "run-9", 77);
        backend.fail_always(
            body.key,
            BodyError::new(BodyErrorKind::Missing, "no such blob"),
        );
        completed(registry, BodyText::deferred(body));
    });
    rig.run();
    rig.read();

    assert!(rig.has("The stored content is missing (no such blob)"));
    assert!(!rig.has("retrying"));
    rig.click("Copy details");
    let copied = rig.harness.state().copied.last().cloned().expect("copied");
    let key = FakeBackend::key_of(&secret);
    assert!(copied.contains(&key.hash_hex()));
    assert!(copied.contains(&format!("byte_len: {}", key.byte_len)));
    assert!(copied.contains("run: run-9"));
    assert!(copied.contains("event: 77"));
    assert!(copied.contains("evorch inspect blob"));
    assert!(copied.contains("missing"));
    assert!(
        !copied.contains("hunter2"),
        "the body is not in the details"
    );
    assert!(!copied.contains('\u{e000}'), "nor a token");
}

#[test]
fn retrying_by_hand_reads_a_body_that_could_not_be_read() {
    let text = text_of(700);
    let mut rig = rig_with(|backend, registry| {
        let body = backend.deferred(&text, RUN, 5);
        backend.fail_always(
            body.key,
            BodyError::new(BodyErrorKind::Corrupt, "hash mismatch"),
        );
        completed(registry, BodyText::deferred(body));
    });
    rig.run();
    rig.read();
    assert!(rig.has("The stored content is damaged (hash mismatch)"));

    rig.backend.stop_failing(&FakeBackend::key_of(&text));
    rig.click("Retry");
    rig.read();

    assert!(rig.has("word word"));
    assert!(!rig.has("Couldn't load content"));
    assert_eq!(rig.backend.bodies_read(), 2);
}

#[test]
fn the_same_body_in_two_places_is_one_read() {
    let mut rig = rig_with(|backend, registry| {
        let key = backend.add(&text_of(600));
        completed(
            registry,
            BodyText::deferred(gui::body::DeferredBody::new(key, Some(RUN), Some(1))),
        );
        completed(
            registry,
            BodyText::deferred(gui::body::DeferredBody::new(key, Some(RUN), Some(2))),
        );
    });
    rig.run();
    assert_eq!(
        rig.harness
            .query_all_by_label_contains("Loading content")
            .count(),
        2
    );
    rig.read();
    assert_eq!(rig.backend.bodies_read(), 1);
    assert_eq!(
        rig.harness.query_all_by_label_contains("word word").count(),
        2
    );
}

#[test]
fn text_that_is_not_in_view_is_not_read_and_a_request_dropped_before_it_started_is_cancelled() {
    let mut rig = rig_with(|backend, registry| {
        for n in 0..60 {
            completed(
                registry,
                BodyText::deferred(backend.deferred(&format!("{n} {}", text_of(400)), RUN, n)),
            );
        }
    });
    rig.harness.state_mut().virtualize = true;
    rig.run();
    let wanted = rig.backend.calls();
    assert!(wanted.is_empty());
    rig.resolver().pump_all();
    let read: usize = rig.backend.bodies_read();
    assert!(
        read < 60,
        "only the bodies on screen were requested ({read})"
    );
    assert!(read > 0);
}

#[test]
fn thinking_and_tool_output_and_compaction_summaries_are_read_only_once_opened() {
    let mut rig = rig_with(|backend, registry| {
        registry.apply_with_body(
            &Event::new(MessageEvent::ReasoningDelta {
                delta: String::new(),
                run_id: Some(RUN.into()),
            }),
            Some(&BodyText::deferred(backend.deferred(&text_of(500), RUN, 1))),
        );
        // The response is over, so the reasoning is no longer open as it streams.
        registry.apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "All done.".into(),
        }));
        registry.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            input: Some(serde_json::json!({"path": "src/lib.rs"})),
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }));
        registry.apply_with_body(
            &Event::new(ToolEvent::ToolCompleted {
                tool_name: "read".into(),
                call_id: "call-1".into(),
                is_error: false,
                output: Some(String::new()),
                detail: None,
                run_id: Some(RUN.into()),
                parent_call_id: None,
            }),
            Some(&BodyText::deferred(backend.deferred(
                &"line of output\n".repeat(300),
                RUN,
                2,
            ))),
        );
        registry.apply_with_body(
            &Event::new(CompactionEvent::Compacted {
                run_id: RUN.into(),
                reason: CompactionReason::Automatic,
                threshold: 0.8,
                context_window_tokens: 200_000,
                window_source: Default::default(),
                estimated_tokens_before: 160_000,
                estimated_tokens_after: 20_000,
                compacted_range_start: 0,
                compacted_range_end: 40,
                checkpoint_id: "cp-1".into(),
                summary: String::new(),
            }),
            Some(&BodyText::deferred(backend.deferred(
                &"summary of work ".repeat(100),
                RUN,
                3,
            ))),
        );
    });
    rig.run();
    rig.resolver().pump_all();
    rig.run();
    assert!(
        rig.backend.calls().is_empty(),
        "nothing opened, nothing read"
    );

    rig.click("thinking");
    rig.read();
    assert_eq!(rig.backend.bodies_read(), 1);
    assert!(rig.has("word word"));

    rig.click("details");
    rig.read();
    assert_eq!(rig.backend.bodies_read(), 2);
    assert!(rig.has("summary of work"));

    rig.harness.get_by_label_contains("lib.rs").click();
    rig.run();
    assert!(rig.has("Loading content"));
    rig.read();
    assert_eq!(rig.backend.bodies_read(), 3);
    assert!(rig.has("line of output"));
}

#[test]
fn a_token_is_never_drawn_whatever_state_the_body_is_in() {
    let mut rig = rig_with(|backend, registry| {
        let body = backend.deferred(&text_of(600), RUN, 5);
        backend.fail_first(body.key, &busy(), 1);
        completed(registry, BodyText::deferred(body));
        // A user's text that happens to look like a token is still just text.
        registry.apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "see \u{e000}evorch-body:v1:abc for details".into(),
        }));
    });
    // The one node that may hold the escape character is the user's own text.
    let nothing_leaks = |rig: &Rig| {
        assert_eq!(
            rig.harness.query_all_by_label_contains("\u{e000}").count(),
            1
        );
        assert_eq!(
            rig.harness
                .query_all_by_label_contains("evorch-body:")
                .count(),
            1
        );
    };
    rig.run();
    nothing_leaks(&rig);
    rig.read();
    nothing_leaks(&rig);
    rig.clock.advance(Duration::from_millis(250));
    rig.read();
    nothing_leaks(&rig);
    assert!(rig.has("word word"));
}

#[test]
fn an_unreadable_event_stands_between_its_neighbours_and_reading_it_again_fills_the_gap() {
    let mut rig = rig_with(|_, registry| {
        registry.apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "first answer".into(),
        }));
        registry.push_unreadable(
            12,
            Some(RUN),
            gui::body::FailureKind::Unreadable,
            "the payload does not decode",
        );
        registry.apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "last answer".into(),
        }));
    });
    rig.run();
    assert!(rig.has("Couldn't read event #12"));
    assert!(rig.has("the payload does not decode"));
    assert!(rig.top("first answer") < rig.top("Couldn't read event #12"));
    assert!(rig.top("Couldn't read event #12") < rig.top("last answer"));
    assert_eq!(
        rig.harness
            .query_all_by_label_contains("Couldn't read event")
            .count(),
        1,
        "one gap, though the run's thread and the run each have a copy"
    );

    rig.click("Copy details");
    let copied = rig.harness.state().copied.last().cloned().unwrap();
    assert!(copied.contains("event: 12") && copied.contains("run: run-1"));

    rig.backend.script_event(
        12,
        EventReread::Ready(DeferredEvent {
            id: 12,
            event: Event::new(MessageEvent::MessageCompleted {
                run_id: RUN.into(),
                text: "the middle answer".into(),
            }),
            body: None,
        }),
    );
    rig.click("Retry");
    assert!(
        rig.has("Reading the event again"),
        "the gap shows the read is under way"
    );
    rig.resolver().pump_all();
    for (id, outcome) in rig.resolver().take_event_results() {
        rig.harness.state_mut().registry.apply_reread(id, &outcome);
    }
    rig.run();

    assert!(!rig.has("Couldn't read event"));
    assert!(rig.top("first answer") < rig.top("the middle answer"));
    assert!(rig.top("the middle answer") < rig.top("last answer"));
}

#[test]
fn the_worker_thread_does_the_reading_and_the_pane_never_waits_for_it() {
    let backend = FakeBackend::new();
    let (wake, woken) = std::sync::mpsc::channel();
    let resolver = BodyResolver::spawn(
        backend.clone(),
        ResolverConfig::default(),
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let mut registry_value = registry();
    completed(
        &mut registry_value,
        BodyText::deferred(backend.deferred(&text_of(600), RUN, 5)),
    );
    let mut harness = Harness::builder()
        .with_size(egui::vec2(720.0, 480.0))
        .build_ui_state(
            |ui, state: &mut State| {
                gui::theme::install(ui.ctx());
                gui::body::ui::install(ui.ctx(), state.resolver.clone());
                transcript_body(ui, state.registry.thread());
                state.resolver.finish_frame();
            },
            State {
                registry: registry_value,
                resolver,
                copied: Vec::new(),
                virtualize: false,
            },
        );

    harness.run_steps(8);
    woken.recv().expect("the worker announces what it read");
    harness.run_steps(8);

    assert!(harness.query_by_label_contains("word word").is_some());
    let readers = backend.reader_threads();
    assert_eq!(readers.len(), 1);
    assert_ne!(
        readers[0],
        std::thread::current().id(),
        "the pane's thread reads nothing"
    );
}

/// Saves the pane as `<dir>/<name>.png`; `false` when there is no GPU adapter.
fn shoot(rig: &mut Rig, dir: &std::path::Path, name: &str) -> bool {
    rig.run();
    // The renderer panics, rather than returning an error, without an adapter.
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.harness.render()))
        .unwrap_or_else(|payload| {
            Err(payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                })
                .unwrap_or_default())
        });
    match rendered {
        Ok(image) => {
            image.save(dir.join(format!("{name}.png"))).expect("save");
            true
        }
        Err(message) if message.to_lowercase().contains("no adapter") => {
            eprintln!("screenshots skipped: {message}");
            false
        }
        Err(message) => panic!("render failed: {message}"),
    }
}

/// Every state of a body, one picture each, taken from the real pane.
/// `EVORCH_DEFERRED_BODIES_EVIDENCE` names the directory they are saved in.
#[test]
#[ignore = "writes PNG evidence of each loading state; needs an offscreen GPU adapter"]
fn capture_every_loading_state() {
    let dir = std::env::var_os("EVORCH_DEFERRED_BODIES_EVIDENCE")
        .map_or_else(|| std::env::temp_dir().join("deferred-bodies"), Into::into);
    std::fs::create_dir_all(&dir).unwrap();
    let frame = |backend: &FakeBackend, registry: &mut TranscriptRegistry, text: &str, id| {
        registry.push_thread(TranscriptEntry::Notice {
            text: "Before the answer".into(),
        });
        let body = backend.deferred(text, RUN, id);
        registry.push_thread(TranscriptEntry::UserMessage {
            text: "Summarise the change".into(),
            skills: Vec::new(),
        });
        completed(registry, BodyText::deferred(body.clone()));
        registry.push_thread(TranscriptEntry::Notice {
            text: "After the answer".into(),
        });
        body
    };
    let answer = "The deferred body is read on a worker thread.\n\n".repeat(12);
    let size = egui::vec2(760.0, 420.0);

    // Waiting, retrying, failing for good, and reading by hand.
    let mut body = None;
    let mut rig = rig_sized(size, |backend, registry| {
        let made = frame(backend, registry, &answer, 5);
        backend.fail_always(made.key, busy());
        body = Some(made);
    });
    if !shoot(&mut rig, &dir, "1-loading") {
        return;
    }
    rig.harness.state().resolver.pump_all();
    shoot(&mut rig, &dir, "2-retrying");
    for delay in [250, 1_000, 4_000] {
        rig.clock.advance(Duration::from_millis(delay));
        rig.harness.state().resolver.pump_all();
    }
    shoot(&mut rig, &dir, "3-failed-after-retries");
    let key = body.unwrap().key;
    rig.backend.stop_failing(&key);
    rig.click("Retry");
    rig.read();
    shoot(&mut rig, &dir, "4-loaded");

    // The stored data itself is wrong.
    let mut rig = rig_sized(size, |backend, registry| {
        let made = frame(backend, registry, &answer, 6);
        backend.fail_always(
            made.key,
            BodyError::new(BodyErrorKind::Missing, "no such blob"),
        );
    });
    rig.run();
    rig.read();
    shoot(&mut rig, &dir, "5-failed-integrity");

    // An event that could not be read, between its neighbours.
    let mut rig = rig_sized(size, |_, registry| {
        registry.apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "The answer before the gap".into(),
        }));
        registry.push_unreadable(
            12,
            Some(RUN),
            gui::body::FailureKind::Unreadable,
            "the payload does not decode",
        );
        registry.apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: RUN.into(),
            text: "The answer after the gap".into(),
        }));
    });
    shoot(&mut rig, &dir, "6-unreadable-event");

    // A thread that was deleted shows neither its text nor a failure.
    let mut rig = rig_sized(size, |backend, registry| {
        let made = frame(backend, registry, &answer, 7);
        backend.fail_always(
            made.key,
            BodyError::new(BodyErrorKind::Missing, "no such blob"),
        );
    });
    rig.run();
    rig.read();
    let runs = rig.harness.state_mut().registry.drop_thread("thread");
    for run in &runs {
        rig.resolver().forget_run(run);
    }
    rig.harness
        .state_mut()
        .registry
        .select_thread(Some("thread".into()));
    shoot(&mut rig, &dir, "7-thread-deleted");
    println!("{}", dir.display());
}
