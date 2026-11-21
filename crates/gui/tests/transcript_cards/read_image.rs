//! A `read` call that returned an image shows that image in its card, drawn
//! from the original file by the bounded loader the previews already use.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::body::{BodyResolver, ManualClock, ResolverConfig, StorageBodies};
use gui::fixture::DemoSource;
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::transcript_body;
use gui::panes::file_viewer::{image_load_started_for_tests, wait_for_image_for_tests};
use serde_json::json;
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

const RUN: &str = "run-1";

fn write_png(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    image::RgbaImage::from_pixel(32, 16, image::Rgba([200, 30, 30, 255]))
        .save(&path)
        .unwrap();
    path
}

fn image_detail(path: &Path) -> serde_json::Value {
    json!({
        "path": path.to_string_lossy(),
        "media_type": "image/png",
        "width": 32,
        "height": 16,
        "bytes": 100,
    })
}

fn tool_events(call: &str, path: &Path, detail: Option<serde_json::Value>) -> [Event; 2] {
    [
        Event::new(ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: call.into(),
            input: Some(json!({ "path": path.to_string_lossy() })),
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: call.into(),
            is_error: false,
            output: Some(format!("Image {}", path.display())),
            detail,
            run_id: Some(RUN.into()),
            parent_call_id: None,
        }),
    ]
}

fn model_with(events: impl IntoIterator<Item = Event>) -> TranscriptModel {
    let mut model = TranscriptModel::new();
    for event in events {
        model.apply(&event);
    }
    model
}

fn harness(model: TranscriptModel) -> Harness<'static, TranscriptModel> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .build_ui_state(
            |ui, model: &mut TranscriptModel| {
                gui::theme::install(ui.ctx());
                transcript_body(ui, model);
            },
            model,
        );
    harness.run_steps(3);
    harness
}

fn finish_image(harness: &mut Harness<'_, TranscriptModel>, path: &Path) {
    wait_for_image_for_tests(&harness.ctx, path);
    harness.run_steps(3);
}

#[test]
fn a_live_image_read_shows_the_image_in_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "shot.png");
    let mut harness = harness(model_with(tool_events(
        "c1",
        &path,
        Some(image_detail(&path)),
    )));

    finish_image(&mut harness, &path);

    assert!(harness.query_by_label("shot.png preview").is_some());
    assert!(harness.query_by_label_contains("Read ").is_some());
    assert!(
        harness
            .query_by_label_contains("Preview unavailable")
            .is_none()
    );
}

#[test]
fn a_missing_image_file_leaves_a_note_and_the_card_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gone.png");
    let mut harness = harness(model_with(tool_events(
        "c1",
        &path,
        Some(image_detail(&path)),
    )));

    finish_image(&mut harness, &path);

    assert!(
        harness
            .query_by_label_contains("Preview unavailable")
            .is_some()
    );
    assert!(harness.query_by_label("gone.png preview").is_none());
    assert!(harness.query_by_label_contains("Read ").is_some());
}

#[test]
fn a_file_that_is_not_an_image_leaves_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake.png");
    std::fs::write(&path, b"this is not a picture").unwrap();
    let mut harness = harness(model_with(tool_events(
        "c1",
        &path,
        Some(image_detail(&path)),
    )));

    finish_image(&mut harness, &path);

    assert!(
        harness
            .query_by_label_contains("Preview unavailable")
            .is_some()
    );
    assert!(harness.query_by_label("fake.png preview").is_none());
}

#[test]
fn a_text_read_or_a_read_without_detail_loads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "plain.png");
    let text_detail = json!({ "path": path.to_string_lossy(), "media_type": "text/plain" });
    let mut events = Vec::from(tool_events("c1", &path, Some(text_detail)));
    events.extend(tool_events("c2", &path, None));
    let harness = harness(model_with(events));

    assert!(!image_load_started_for_tests(&harness.ctx, &path));
    assert!(harness.query_by_label_contains("preview").is_none());
    assert!(
        harness
            .query_by_label_contains("Preview unavailable")
            .is_none()
    );
}

#[test]
fn an_image_inside_a_closed_group_loads_only_once_the_group_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "folded.png");
    let other = dir.path().join("other.rs");
    let mut events = Vec::from(tool_events("c1", &path, Some(image_detail(&path))));
    events.extend(tool_events("c2", &other, None));
    events.push(Event::new(MessageEvent::MessageDelta {
        run_id: Some(RUN.into()),
        delta: "All done.".into(),
    }));
    let mut harness = harness(model_with(events));

    assert!(harness.query_by_label_contains("2 tool calls").is_some());
    assert!(!image_load_started_for_tests(&harness.ctx, &path));

    harness.get_by_label_contains("2 tool calls").click();
    harness.run_steps(3);
    finish_image(&mut harness, &path);

    assert!(image_load_started_for_tests(&harness.ctx, &path));
    assert!(harness.query_by_label("folded.png preview").is_some());
}

#[test]
fn an_image_read_inside_run_js_shows_the_image_in_its_child_card() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "nested.png");
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "run_js".into(),
        call_id: "js".into(),
        input: Some(json!({ "code": "await read({path: 'nested.png'});" })),
        run_id: Some(RUN.into()),
        parent_call_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "read".into(),
        call_id: "js/1".into(),
        input: Some(json!({ "path": path.to_string_lossy() })),
        run_id: Some(RUN.into()),
        parent_call_id: Some("js".into()),
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".into(),
        call_id: "js/1".into(),
        is_error: false,
        output: Some("Image".into()),
        detail: Some(image_detail(&path)),
        run_id: Some(RUN.into()),
        parent_call_id: Some("js".into()),
    }));
    let mut harness = harness(model);

    finish_image(&mut harness, &path);

    assert!(harness.query_by_label("nested.png preview").is_some());
}

struct Log {
    dir: tempfile::TempDir,
    config: StorageConfig,
}

fn stored_log(image: &Path, dir: tempfile::TempDir) -> Log {
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut events: Vec<EventKind> = vec![
        LifecycleEvent::AgentRunStarted {
            run_id: RUN.into(),
            parent_run_id: None,
            agent_name: "chat:Worker:t1".into(),
            role: "Worker".into(),
        }
        .into(),
    ];
    events.extend(
        tool_events("call-1", image, Some(image_detail(image)))
            .into_iter()
            .map(|event| event.kind),
    );
    events.push(
        LifecycleEvent::AgentRunStateChanged {
            run_id: RUN.into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
            waiting: None,
        }
        .into(),
    );
    for kind in events {
        storage
            .handle()
            .append_event(Some("gui"), &Event::new(kind))
            .unwrap();
    }
    storage.close();
    Log { dir, config }
}

fn restored(log: &Log, resolver: &BodyResolver) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", log.dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("t1"), project, "t1")
        .unwrap();
    sidebar.threads[0].run_ids.push(RUN.into());
    sidebar.switch_thread(&ThreadId::new("t1")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_body_resolver(resolver.clone());
    state
        .restore_history(&Database::open(&log.config).unwrap())
        .unwrap();
    state
}

struct Pane {
    state: WorkbenchState<DemoSource>,
    resolver: BodyResolver,
}

#[test]
fn a_restored_image_read_shows_the_image_in_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_png(dir.path(), "history.png");
    let log = stored_log(&path, dir);
    let resolver = BodyResolver::manual(
        Arc::new(StorageBodies::new(log.config.db_path.clone())),
        ResolverConfig {
            clock: Arc::new(ManualClock::new()),
            ..ResolverConfig::default()
        },
    );
    let state = restored(&log, &resolver);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(760.0, 480.0))
        .build_ui_state(
            |ui, pane: &mut Pane| {
                gui::theme::install(ui.ctx());
                gui::body::ui::install(ui.ctx(), pane.resolver.clone());
                transcript_body(ui, pane.state.transcript());
                pane.resolver.finish_frame();
            },
            Pane { state, resolver },
        );
    harness.run_steps(3);

    wait_for_image_for_tests(&harness.ctx, &path);
    harness.run_steps(3);

    assert!(harness.query_by_label("history.png preview").is_some());
}
