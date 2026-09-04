use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui::app::WorkbenchState;
use gui::diff::{
    DIFF_BYTE_CAP, DiffError, DiffMode, DiffRequest, DiffSource, DiffState, FixtureDiffSource,
};
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::AgentSummary;
use workspace_ui::{PanelId, ProjectId, SidebarState, UiSettings};

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl MockSource {
    fn empty() -> Self {
        Self(Vec::new())
    }
}

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

/// worker の fetch を test 側の signal まで block する source。
struct GatedDiffSource {
    gate: Mutex<Receiver<()>>,
}

impl DiffSource for GatedDiffSource {
    fn fetch(&self, _req: &DiffRequest) -> Result<String, DiffError> {
        let gate = self.gate.lock().expect("gate lock");
        gate.recv().expect("gate released");
        drop(gate);
        Ok("first line\nsecond line".to_owned())
    }
}

#[derive(Debug, Default)]
struct RecordingDiffSource {
    modes: Mutex<Vec<DiffMode>>,
}

impl DiffSource for RecordingDiffSource {
    fn fetch(&self, req: &DiffRequest) -> Result<String, DiffError> {
        self.modes
            .lock()
            .expect("modes lock")
            .push(req.mode.clone());
        Ok("branch body".to_owned())
    }
}

fn state_with_diff(source: Arc<dyn DiffSource>, root: &Path) -> WorkbenchState<MockSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar
        .add_project(project.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project)
        .expect("project can be selected");
    WorkbenchState::new(MockSource::empty(), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_diff_source(source)
}

fn diff_harness(source: Arc<dyn DiffSource>, root: &Path) -> HeadlessWorkbench<MockSource> {
    let mut harness = HeadlessWorkbench::new(state_with_diff(source, root), [800.0, 600.0]);
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("diff-main"))
        .expect("diff tab exists");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("diff tab can be activated");
    harness.run();
    harness
}

fn step_until(harness: &mut HeadlessWorkbench<MockSource>, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !harness.has_label(label) && Instant::now() < deadline {
        harness.step();
        std::thread::yield_now();
    }
    assert!(harness.has_label(label), "label {label:?} did not appear");
}

#[test]
fn diff_pane_shows_loading_then_ready_from_fixture() {
    // Given: a diff tab wired to a source whose fetch blocks on a test gate
    let temp = tempfile::tempdir().expect("temp dir");
    let (tx, rx) = mpsc::channel();
    let source = Arc::new(GatedDiffSource {
        gate: Mutex::new(rx),
    });
    let mut harness = diff_harness(source, temp.path());

    // When: the working tree diff is requested from the pane
    harness.click_label("Working tree");
    harness.step();

    // Then: the model is loading before the worker completes
    assert!(matches!(
        harness.state().diff().state(&DiffMode::WorkingTree),
        DiffState::Loading
    ));
    harness.step();
    assert!(harness.has_label("loading…"));

    // When: the worker result is released and frames settle
    tx.send(()).expect("release gate");
    step_until(&mut harness, "first line\nsecond line");

    // Then: the ready body renders the diff text
    assert!(matches!(
        harness.state().diff().state(&DiffMode::WorkingTree),
        DiffState::Ready { .. }
    ));
}

#[test]
fn empty_diff_shows_explicit_empty_state() {
    // Given: a fixture source that reports no changes
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = diff_harness(Arc::new(FixtureDiffSource::empty()), temp.path());

    // When: the working tree diff is requested
    harness.click_label("Working tree");

    // Then: the empty state is explicit
    step_until(&mut harness, "no changes");
    assert!(matches!(
        harness.state().diff().state(&DiffMode::WorkingTree),
        DiffState::Empty
    ));
}

#[test]
fn truncated_diff_shows_cap_notice() {
    // Given: a fixture diff larger than the byte cap
    let temp = tempfile::tempdir().expect("temp dir");
    let total_bytes = DIFF_BYTE_CAP + 1;
    let text = "x".repeat(total_bytes);
    let mut harness = diff_harness(Arc::new(FixtureDiffSource::ready(&text)), temp.path());

    // When: the working tree diff is requested
    harness.click_label("Working tree");

    // Then: the truncation notice reports cap and total
    step_until(
        &mut harness,
        &format!("truncated: showing {DIFF_BYTE_CAP} of {total_bytes} bytes"),
    );
    assert!(matches!(
        harness.state().diff().state(&DiffMode::WorkingTree),
        DiffState::Truncated { .. }
    ));
}

#[test]
fn git_error_is_shown_and_ui_keeps_rendering() {
    // Given: a fixture source that fails
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = diff_harness(Arc::new(FixtureDiffSource::error("boom")), temp.path());

    // When: the working tree diff is requested
    harness.click_label("Working tree");

    // Then: the error is shown without blocking the rest of the workbench
    step_until(&mut harness, "error: diff output I/O error: boom");
    harness.step();
    assert!(harness.has_label("Projects"));
}

#[test]
fn branch_mode_requests_main_merge_base() {
    // Given: a recording source behind the diff tab
    let temp = tempfile::tempdir().expect("temp dir");
    let source = Arc::new(RecordingDiffSource::default());
    let mut harness = diff_harness(source.clone(), temp.path());

    // When: the branch diff button is clicked
    harness.click_label("Branch vs main");

    // Then: the model fetches a branch diff against main
    step_until(&mut harness, "branch body");
    assert_eq!(
        source.modes.lock().expect("modes lock").as_slice(),
        &[DiffMode::Branch {
            base: "main".to_owned()
        }]
    );
}
