//! Headless transcript benchmark: history restore, first open, scrolling,
//! streaming, width change and (once grouping exists) group expansion, over
//! seeded synthetic histories. Report-only: no number here is a pass criterion.
//!
//! ```text
//! cargo run --release -p gui --example transcript_perf -- \
//!     [--runs 5] [--sizes 1000,5000,10000] [--shapes mixed,poll,...] \
//!     [--phases restore,open,scroll,stream,width,expand] \
//!     [--modes production,accesskit] [--eviction]
//! ```
//!
//! What it measures, and what it does not:
//! - CPU cost of egui layout and widget building through `egui_kittest`.
//!   It does not measure tessellation, GPU upload or presentation.
//! - `production` forces the virtualized path (the one a window without a
//!   screen reader takes). kittest enables AccessKit, which makes the pane
//!   draw every row, so `accesskit` leaves that behaviour as it is. The
//!   `rows_skipped` counter shows which path a row took. Both modes also pay
//!   for building the AccessKit tree of the widgets they draw, which a
//!   production window without a screen reader does not.
//! - The transcript model caps at 10,000 entries. `--eviction` adds 20,000
//!   seeded entries, labelled "10k shown + eviction path".
//! - Timings are wall clock over `--runs` runs of the same command, reported
//!   as median / p95 (nearest rank); work counters come from the last run.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui_kittest::Harness;
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::body::testing::FakeBackend;
use gui::body::{BodyResolver, BodyText, ManualClock, ResolverConfig};
use gui::fixture::DemoSource;
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::{
    TranscriptPerf, set_transcript_perf_enabled, set_transcript_virtualization_for_tests,
    take_transcript_perf, transcript_body,
};
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

const SIZE: [f32; 2] = [900.0, 700.0];
const NARROW: [f32; 2] = [640.0, 700.0];
const SETTLE_LIMIT: usize = 400;
const STREAM_FRAMES: usize = 60;
const BIG_BODY: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    Mixed,
    Poll,
    RunJs,
    MultiRun,
    MessageOnly,
    Deferred,
    Tail,
}

impl Shape {
    const ALL: [Self; 7] = [
        Self::Mixed,
        Self::Poll,
        Self::RunJs,
        Self::MultiRun,
        Self::MessageOnly,
        Self::Deferred,
        Self::Tail,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Poll => "poll",
            Self::RunJs => "run_js",
            Self::MultiRun => "multirun",
            Self::MessageOnly => "message_only",
            Self::Deferred => "deferred",
            Self::Tail => "tail",
        }
    }

    /// Entries one cycle adds to the transcript.
    fn per_cycle(self) -> usize {
        match self {
            Self::Mixed | Self::MultiRun | Self::Deferred | Self::Tail => 7,
            Self::Poll => 8,
            Self::RunJs => 6,
            Self::MessageOnly => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Production,
    AccessKit,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::AccessKit => "accesskit",
        }
    }
}

/// Deterministic pseudo random numbers, so every run sees the same history.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % bound.max(1)
    }
}

fn prose(rng: &mut Lcg, words: usize) -> String {
    const WORDS: [&str; 12] = [
        "the",
        "transcript",
        "row",
        "measure",
        "layout",
        "cache",
        "height",
        "frame",
        "tool",
        "result",
        "message",
        "stable",
    ];
    let mut text = String::new();
    for index in 0..words {
        if index > 0 {
            text.push(if index % 23 == 0 { '\n' } else { ' ' });
            if index % 23 == 0 {
                text.push('\n');
            }
        }
        text.push_str(WORDS[rng.next(WORDS.len())]);
    }
    text
}

fn tool_started(
    run: &str,
    name: &str,
    call: &str,
    parent: Option<&str>,
    input: serde_json::Value,
) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input: Some(input),
        run_id: Some(run.into()),
        parent_call_id: parent.map(str::to_owned),
    })
}

fn tool_completed(
    run: &str,
    name: &str,
    call: &str,
    parent: Option<&str>,
    output: String,
) -> Event {
    Event::new(ToolEvent::ToolCompleted {
        tool_name: name.into(),
        call_id: call.into(),
        is_error: false,
        output: Some(output),
        detail: None,
        run_id: Some(run.into()),
        parent_call_id: parent.map(str::to_owned),
    })
}

fn reasoning(run: &str, text: String) -> Event {
    Event::new(MessageEvent::ReasoningDelta {
        run_id: Some(run.into()),
        delta: text,
    })
}

fn message(run: &str, text: String) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text,
    })
}

fn turn_end(run: &str) -> Event {
    Event::new(LifecycleEvent::TurnCompleted {
        run_id: run.into(),
        context_len: 1,
    })
}

const RUNS: [&str; 4] = ["root", "child-1", "child-2", "child-3"];

/// The events of `target` entries of `shape`, from a fixed seed. The root run
/// and the child runs start first, as a persisted chat thread does.
fn history(shape: Shape, target: usize) -> Vec<Event> {
    let mut rng = Lcg(0x5eed_1234 + shape as u64);
    let mut events = vec![Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "root".into(),
        parent_run_id: None,
        agent_name: "chat:Worker:one".into(),
        role: "Worker".into(),
    })];
    for child in &RUNS[1..] {
        events.push(Event::new(LifecycleEvent::AgentRunStarted {
            run_id: (*child).into(),
            parent_run_id: Some("root".into()),
            agent_name: (*child).into(),
            role: "Worker".into(),
        }));
    }
    let big = shape == Shape::Deferred;
    let mut call = 0_usize;
    let mut next_call = || {
        call += 1;
        format!("call-{call}")
    };
    let cycles = target.div_ceil(shape.per_cycle());
    for cycle in 0..cycles {
        let run = if shape == Shape::MultiRun {
            RUNS[cycle % RUNS.len()]
        } else {
            "root"
        };
        let body =
            |rng: &mut Lcg, small: usize, large: usize| prose(rng, if big { large } else { small });
        match shape {
            Shape::MessageOnly => {
                let words = 40 + rng.next(60);
                events.push(message(run, prose(&mut rng, words)));
            }
            Shape::Poll => {
                events.push(reasoning(run, prose(&mut rng, 30)));
                for _ in 0..3 {
                    let id = next_call();
                    events.push(tool_started(run, "inbox", &id, None, serde_json::json!({})));
                    events.push(tool_completed(run, "inbox", &id, None, "[]".into()));
                }
                let id = next_call();
                events.push(tool_started(
                    run,
                    "wait",
                    &id,
                    None,
                    serde_json::json!({"seconds": 5}),
                ));
                events.push(tool_completed(run, "wait", &id, None, "timed out".into()));
                let id = next_call();
                events.push(tool_started(
                    run,
                    "read",
                    &id,
                    None,
                    serde_json::json!({"path": "src/lib.rs"}),
                ));
                events.push(tool_completed(run, "read", &id, None, prose(&mut rng, 40)));
                events.push(message(run, prose(&mut rng, 50)));
                events.push(turn_end(run));
            }
            Shape::RunJs => {
                events.push(reasoning(run, prose(&mut rng, 30)));
                let parent = next_call();
                events.push(tool_started(
                    run,
                    "run_js",
                    &parent,
                    None,
                    serde_json::json!({"code": "return await read({path: 'a.rs'});"}),
                ));
                for _ in 0..2 {
                    let id = next_call();
                    events.push(tool_started(
                        run,
                        "read",
                        &id,
                        Some(&parent),
                        serde_json::json!({"path": "a.rs"}),
                    ));
                    events.push(tool_completed(
                        run,
                        "read",
                        &id,
                        Some(&parent),
                        prose(&mut rng, 30),
                    ));
                }
                events.push(tool_completed(
                    run,
                    "run_js",
                    &parent,
                    None,
                    prose(&mut rng, 20),
                ));
                events.push(message(run, prose(&mut rng, 50)));
                events.push(turn_end(run));
            }
            Shape::Mixed | Shape::MultiRun | Shape::Deferred | Shape::Tail => {
                events.push(reasoning(run, prose(&mut rng, 40)));
                let id = next_call();
                events.push(tool_started(
                    run,
                    "read",
                    &id,
                    None,
                    serde_json::json!({"path": "src/a.rs"}),
                ));
                events.push(tool_completed(
                    run,
                    "read",
                    &id,
                    None,
                    body(&mut rng, 60, 3_000),
                ));
                let id = next_call();
                events.push(tool_started(
                    run,
                    "read",
                    &id,
                    None,
                    serde_json::json!({"path": "src/b.rs"}),
                ));
                events.push(tool_completed(
                    run,
                    "read",
                    &id,
                    None,
                    body(&mut rng, 60, 3_000),
                ));
                events.push(reasoning(run, prose(&mut rng, 40)));
                let id = next_call();
                events.push(tool_started(
                    run,
                    "grep",
                    &id,
                    None,
                    serde_json::json!({"pattern": "row"}),
                ));
                events.push(tool_completed(
                    run,
                    "grep",
                    &id,
                    None,
                    body(&mut rng, 30, 2_000),
                ));
                events.push(message(run, body(&mut rng, 60, 1_500)));
                events.push(turn_end(run));
            }
        }
    }
    if shape == Shape::Tail {
        let id = next_call();
        events.push(tool_started(
            "root",
            "shell",
            &id,
            None,
            serde_json::json!({"command": "cargo test"}),
        ));
    }
    events
}

/// The model the events build. With a backend, a large message or tool output
/// is kept apart and shown as a deferred body, as a restored history does.
fn model_of(events: &[Event], backend: Option<&FakeBackend>) -> TranscriptModel {
    let mut model = TranscriptModel::new();
    let mut id = 0_i64;
    for event in events {
        id += 1;
        let Some(backend) = backend else {
            model.apply(event);
            continue;
        };
        let mut event = event.clone();
        let (run, text): (String, Option<&mut String>) = match &mut event.kind {
            event_bus::EventKind::Message(MessageEvent::MessageCompleted { run_id, text }) => {
                (run_id.clone(), Some(text))
            }
            event_bus::EventKind::Tool(ToolEvent::ToolCompleted {
                run_id,
                output: Some(output),
                ..
            }) => (run_id.clone().unwrap_or_default(), Some(output)),
            _ => (String::new(), None),
        };
        match text {
            Some(text) if text.len() >= BIG_BODY => {
                let full = std::mem::take(text);
                let body = BodyText::deferred(backend.deferred(&full, &run, id));
                model.apply_with_body(&event, Some(body));
            }
            _ => model.apply(&event),
        }
    }
    model
}

struct State {
    model: TranscriptModel,
    force: Option<bool>,
    resolver: Option<BodyResolver>,
}

fn draw(ui: &mut egui::Ui, state: &mut State) {
    gui::theme::install(ui.ctx());
    if let Some(force) = state.force {
        set_transcript_virtualization_for_tests(ui.ctx(), force);
    }
    if let Some(resolver) = &state.resolver {
        gui::body::ui::install(ui.ctx(), resolver.clone());
    }
    transcript_body(ui, &state.model);
    if let Some(resolver) = &state.resolver {
        resolver.finish_frame();
    }
}

#[derive(Default, Clone)]
struct Frames {
    ms: Vec<f64>,
    perf: TranscriptPerf,
}

impl Frames {
    fn add(&mut self, ms: f64, perf: TranscriptPerf) {
        self.ms.push(ms);
        self.perf = sum(self.perf, perf);
    }

    fn total(&self) -> f64 {
        self.ms.iter().sum()
    }
}

fn sum(a: TranscriptPerf, b: TranscriptPerf) -> TranscriptPerf {
    TranscriptPerf {
        passes: a.passes + b.passes,
        entries_enumerated: a.entries_enumerated + b.entries_enumerated,
        rows_listed: a.rows_listed + b.rows_listed,
        rows_drawn: a.rows_drawn + b.rows_drawn,
        rows_skipped: a.rows_skipped + b.rows_skipped,
        rows_drawn_far: a.rows_drawn_far + b.rows_drawn_far,
        rows_unsettled: a.rows_unsettled + b.rows_unsettled,
        revisions: a.revisions + b.revisions,
        projection_rebuilds: a.projection_rebuilds + b.projection_rebuilds,
        height_cache_clears: a.height_cache_clears + b.height_cache_clears,
        rows_hidden_by_groups: a.rows_hidden_by_groups + b.rows_hidden_by_groups,
        group_headers: a.group_headers + b.group_headers,
        prep_ns: a.prep_ns + b.prep_ns,
        revision_ns: a.revision_ns + b.revision_ns,
        draw_ns: a.draw_ns + b.draw_ns,
        loop_ns: a.loop_ns + b.loop_ns,
    }
}

struct Rig {
    harness: Harness<'static, State>,
}

impl Rig {
    fn build(shape: Shape, events: &[Event], mode: Mode) -> (Self, Frames) {
        let (backend, resolver) = if shape == Shape::Deferred {
            let backend = FakeBackend::new();
            let resolver = BodyResolver::manual(
                backend.clone(),
                ResolverConfig {
                    clock: Arc::new(ManualClock::new()),
                    ..ResolverConfig::default()
                },
            );
            (Some(backend), Some(resolver))
        } else {
            (None, None)
        };
        let model = model_of(events, backend.as_deref());
        let state = State {
            model,
            force: (mode == Mode::Production).then_some(true),
            resolver,
        };
        take_transcript_perf();
        // The builder draws the first frame itself.
        let started = Instant::now();
        let harness = Harness::builder()
            .with_size(egui::vec2(SIZE[0], SIZE[1]))
            .build_ui_state(draw, state);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut frames = Frames::default();
        frames.add(ms, take_transcript_perf());
        (Self { harness }, frames)
    }

    fn frame(&mut self) -> (f64, TranscriptPerf) {
        if let Some(resolver) = &self.harness.state().resolver {
            resolver.pump_all();
        }
        take_transcript_perf();
        let started = Instant::now();
        self.harness.step();
        (
            started.elapsed().as_secs_f64() * 1000.0,
            take_transcript_perf(),
        )
    }

    /// Frames until a frame draws no row whose height is still unsettled.
    fn settle(&mut self) -> Frames {
        let mut frames = Frames::default();
        for _ in 0..SETTLE_LIMIT {
            let (ms, perf) = self.frame();
            let done = perf.rows_unsettled == 0 && perf.passes > 0;
            frames.add(ms, perf);
            if done && frames.ms.len() >= 2 {
                break;
            }
        }
        frames
    }

    fn resolved(&self) -> (u64, u64) {
        self.harness
            .state()
            .resolver
            .as_ref()
            .map_or((0, 0), |resolver| {
                let stats = resolver.stats();
                (stats.bodies_read, stats.bytes_read)
            })
    }

    fn scroll_gesture(&mut self, delta: f32, frames: &mut Frames) {
        self.harness.hover_at(egui::pos2(320.0, 240.0));
        let (ms, perf) = self.frame();
        frames.add(ms, perf);
        for (phase, amount) in [
            (egui::TouchPhase::Start, 0.0),
            (egui::TouchPhase::Move, delta),
            (egui::TouchPhase::End, 0.0),
        ] {
            self.harness.event(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, amount),
                phase,
                modifiers: egui::Modifiers::NONE,
            });
            let (ms, perf) = self.frame();
            frames.add(ms, perf);
        }
        let (ms, perf) = self.frame();
        frames.add(ms, perf);
    }
}

/// One measured step of a case, from one run.
#[derive(Default, Clone)]
struct Sample {
    frames: Frames,
    bodies: u64,
    bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Open,
    Scroll,
    Stream,
    Width,
    Expand,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Scroll => "scroll",
            Self::Stream => "stream",
            Self::Width => "width",
            Self::Expand => "expand",
        }
    }
}

/// Measures one phase once. Returns the labelled samples it produced.
fn measure(
    shape: Shape,
    events: &[Event],
    mode: Mode,
    phase: Phase,
) -> Vec<(&'static str, Sample)> {
    let (mut rig, first) = Rig::build(shape, events, mode);
    let resolved = |rig: &Rig, before: (u64, u64), frames: Frames| {
        let now = rig.resolved();
        Sample {
            frames,
            bodies: now.0 - before.0,
            bytes: now.1 - before.1,
        }
    };
    match phase {
        Phase::Open => {
            let before = (0, 0);
            let first_sample = resolved(&rig, before, first);
            let settle_from = rig.resolved();
            let settle = rig.settle();
            let settle_sample = resolved(&rig, settle_from, settle);
            vec![
                ("open: first frame", first_sample),
                ("open: until settled", settle_sample),
            ]
        }
        Phase::Scroll => {
            rig.settle();
            let before = rig.resolved();
            let mut frames = Frames::default();
            for delta in [3_000.0_f32; 8].into_iter().chain([-1_500.0_f32; 4]) {
                rig.scroll_gesture(delta, &mut frames);
            }
            vec![("scroll: 12 gestures", resolved(&rig, before, frames))]
        }
        Phase::Stream => {
            rig.settle();
            let before = rig.resolved();
            let mut rng = Lcg(7);
            let mut frames = Frames::default();
            for _ in 0..STREAM_FRAMES {
                let delta = format!("{} ", prose(&mut rng, 24));
                rig.harness
                    .state_mut()
                    .model
                    .apply(&Event::new(MessageEvent::MessageDelta {
                        run_id: Some("root".into()),
                        delta,
                    }));
                let (ms, perf) = rig.frame();
                frames.add(ms, perf);
            }
            vec![("stream: 60 delta frames", resolved(&rig, before, frames))]
        }
        Phase::Width => {
            rig.settle();
            let before = rig.resolved();
            rig.harness.set_size(egui::vec2(NARROW[0], NARROW[1]));
            let mut frames = Frames::default();
            let (ms, perf) = rig.frame();
            frames.add(ms, perf);
            let first_sample = resolved(&rig, before, frames.clone());
            let settle = rig.settle();
            for (ms, perf) in settle.ms.iter().zip(std::iter::repeat(settle.perf)) {
                let _ = (ms, perf);
            }
            let mut all = frames;
            all.ms.extend(settle.ms.iter().copied());
            all.perf = sum(all.perf, settle.perf);
            vec![
                ("width: first frame", first_sample),
                ("width: until settled", resolved(&rig, before, all)),
            ]
        }
        Phase::Expand => expand(&mut rig),
    }
}

/// Group expansion needs tool groups; before they exist there is nothing to
/// expand and the case reports nothing.
fn expand(_rig: &mut Rig) -> Vec<(&'static str, Sample)> {
    Vec::new()
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn stats(mut values: Vec<f64>) -> (f64, f64) {
    values.sort_by(f64::total_cmp);
    (percentile(&values, 0.5), percentile(&values, 0.95))
}

fn report_case(label: &str, entries: usize, runs: Vec<Sample>) {
    let totals: Vec<f64> = runs.iter().map(|run| run.frames.total()).collect();
    let (median, p95) = stats(totals);
    let per_frame: Vec<f64> = runs
        .iter()
        .flat_map(|run| run.frames.ms.iter().copied())
        .collect();
    let (frame_median, frame_p95) = stats(per_frame);
    let last = runs.last().cloned().unwrap_or_default();
    let perf = last.frames.perf;
    println!(
        "| {label} | {entries} | {} | {median:.2} | {p95:.2} | {frame_median:.3} | {frame_p95:.3} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} |",
        last.frames.ms.len(),
        perf.passes,
        perf.entries_enumerated,
        perf.rows_listed,
        perf.rows_drawn,
        perf.rows_skipped,
        perf.rows_drawn_far,
        perf.projection_rebuilds,
        last.bodies,
        last.bytes,
        perf.prep_ns as f64 / 1e6,
        perf.revision_ns as f64 / 1e6,
        perf.draw_ns as f64 / 1e6,
    );
}

fn header() {
    println!(
        "| case | entries | frames | total median ms | total p95 ms | frame median ms | frame p95 ms | passes | entries enumerated | rows listed | rows drawn | rows skipped | rows drawn far | projection rebuilds | bodies resolved | body bytes | prep ms | revision ms | draw ms |"
    );
    println!("|{}", "---|".repeat(19));
}

// ---- history restore ----

#[derive(Default)]
struct StageLog(Mutex<Vec<(String, f64)>>);

struct StageLayer(Arc<StageLog>);

struct Fields {
    stage: Option<String>,
    elapsed_ms: Option<f64>,
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "stage" {
            self.stage = Some(value.to_owned());
        }
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "elapsed_ms" {
            self.elapsed_ms = Some(value);
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for StageLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "gui::restore" {
            return;
        }
        let mut fields = Fields {
            stage: None,
            elapsed_ms: None,
        };
        event.record(&mut fields);
        if let (Some(stage), Some(ms)) = (fields.stage, fields.elapsed_ms) {
            self.0.0.lock().unwrap().push((stage, ms));
        }
    }
}

fn restore_state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("one"), project, "one")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"))
        .with_provider_status(ProviderStatus::Configured)
}

fn restore_case(shape: Shape, target: usize, runs: usize) {
    use tracing_subscriber::layer::SubscriberExt;
    let events = history(shape, target);
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut seeded = restore_state(dir.path());
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
        seeded.apply_events([event.clone()]);
    }
    seeded.save_sidebar();
    let _ = AgentRunPhase::Done;
    let mut stages: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
    let mut totals = Vec::new();
    let mut restored_entries = 0;
    for _ in 0..runs {
        let log = Arc::new(StageLog::default());
        let subscriber = tracing_subscriber::registry().with(StageLayer(log.clone()));
        let db = Database::open(&config).unwrap();
        let mut state = restore_state(dir.path());
        let started = Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            state.restore_history(&db).unwrap();
        });
        totals.push(started.elapsed().as_secs_f64() * 1000.0);
        restored_entries = state.transcript().entries().len();
        for (stage, ms) in log.0.lock().unwrap().drain(..) {
            stages.entry(stage).or_default().push(ms);
        }
    }
    storage.close();
    let (median, p95) = stats(totals);
    println!(
        "| restore {} | {} events | {restored_entries} | restore_history wall | {median:.2} | {p95:.2} |",
        shape.name(),
        events.len()
    );
    for (stage, values) in stages {
        let (median, p95) = stats(values);
        println!(
            "| restore {} | {} events | {restored_entries} | {stage} | {median:.2} | {p95:.2} |",
            shape.name(),
            events.len()
        );
    }
}

fn main() {
    let mut runs = 5_usize;
    let mut sizes = vec![1_000_usize, 5_000, 10_000];
    let mut shapes = Shape::ALL.to_vec();
    let mut phases = vec!["restore", "open", "scroll", "stream", "width", "expand"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut modes = vec![Mode::Production, Mode::AccessKit];
    let mut eviction = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().expect("missing value");
        match argument.as_str() {
            "--runs" => runs = value().parse().unwrap(),
            "--sizes" => {
                sizes = value()
                    .split(',')
                    .map(|size| size.parse().unwrap())
                    .collect();
            }
            "--shapes" => {
                let wanted = value();
                shapes = Shape::ALL
                    .into_iter()
                    .filter(|shape| wanted.split(',').any(|name| name == shape.name()))
                    .collect();
            }
            "--phases" => phases = value().split(',').map(str::to_owned).collect(),
            "--modes" => {
                let wanted = value();
                modes = [Mode::Production, Mode::AccessKit]
                    .into_iter()
                    .filter(|mode| wanted.split(',').any(|name| name == mode.name()))
                    .collect();
            }
            "--eviction" => eviction = true,
            other => panic!("unknown option {other}"),
        }
    }
    if eviction {
        sizes.push(20_000);
    }
    set_transcript_perf_enabled(true);
    println!(
        "profile: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!(
        "runs per case: {runs}; window {}x{} (then {}x{} for width); CPU layout only, no GPU",
        SIZE[0], SIZE[1], NARROW[0], NARROW[1]
    );
    if phases.iter().any(|phase| phase == "restore") {
        println!("\n## history restore (stage = RestoreTimer, ms)\n");
        println!("| case | events | entries restored | stage | median ms | p95 ms |");
        println!("|---|---|---|---|---|---|");
        for &size in &sizes {
            for &shape in &shapes {
                if shape == Shape::Deferred {
                    continue;
                }
                restore_case(shape, size, runs);
            }
        }
    }
    for mode in &modes {
        for phase in [
            Phase::Open,
            Phase::Scroll,
            Phase::Stream,
            Phase::Width,
            Phase::Expand,
        ] {
            if !phases.iter().any(|name| name == phase.name()) {
                continue;
            }
            println!("\n## {} / {}\n", mode.name(), phase.name());
            header();
            for &size in &sizes {
                for &shape in &shapes {
                    let events = history(shape, size);
                    let entries = model_of(&events, None).entries().len();
                    let mut by_label: Vec<(&'static str, Vec<Sample>)> = Vec::new();
                    for _ in 0..runs {
                        for (index, (label, sample)) in measure(shape, &events, *mode, phase)
                            .into_iter()
                            .enumerate()
                        {
                            if by_label.len() <= index {
                                by_label.push((label, Vec::new()));
                            }
                            by_label[index].1.push(sample);
                        }
                    }
                    let size_label = if size > 10_000 {
                        format!("{} (10k shown + eviction path)", shape.name())
                    } else {
                        shape.name().to_owned()
                    };
                    for (label, samples) in by_label {
                        report_case(&format!("{size_label} {label}"), entries, samples);
                    }
                }
            }
        }
    }
}
