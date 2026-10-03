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

    // When: the tab opens, it requests the working tree without any click
    harness.step();

    // Then: the model is loading before the worker completes
    assert!(matches!(
        harness.state().diff().state(&DiffMode::WorkingTree),
        DiffState::Loading
    ));
    harness.step();
    assert!(harness.has_label("Loading working tree diff…"));

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

    // When: the visible tab requests the working tree automatically

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

    // When: the visible tab requests the working tree automatically

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

    // When: the visible tab requests the working tree automatically

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
    let deadline = Instant::now() + Duration::from_secs(5);
    while !source
        .modes
        .lock()
        .expect("modes lock")
        .contains(&DiffMode::Branch)
        && Instant::now() < deadline
    {
        harness.step();
        std::thread::yield_now();
    }
    assert_eq!(
        source.modes.lock().expect("modes lock").as_slice(),
        &[DiffMode::WorkingTree, DiffMode::Branch]
    );
}

const REVIEW_DIFF: &str = "diff --git a/src/main.rs b/src/main.rs\nindex abc..def 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,3 +10,4 @@ fn main()\n fn main() {\n-    let message = \"old\";\n+    let message = \"new\";\n+    println!(\"{message}\");\n }\ndiff --git a/image.png b/image.png\nBinary files a/image.png and b/image.png differ\n";

fn review_harness(size: egui::Vec2) -> egui_kittest::Harness<'static, gui::diff::DiffModel> {
    let mut model = gui::diff::DiffModel::new();
    model.show_snapshot(REVIEW_DIFF.into());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(size)
        .build_ui_state(
            |ui, model| {
                gui::theme::style::install(ui.ctx());
                gui::panes::diff::diff_pane(ui, model);
            },
            model,
        );
    harness.run_steps(4);
    harness
}

fn painted_texts(
    harness: &egui_kittest::Harness<'_, gui::diff::DiffModel>,
) -> Vec<(String, egui::Pos2)> {
    harness
        .output()
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => Some((text.galley.text().to_owned(), text.pos)),
            _ => None,
        })
        .collect()
}

#[test]
fn review_diff_shows_file_stats_numbers_and_tinted_full_width_rows() {
    use egui_kittest::kittest::Queryable;
    let harness = review_harness(egui::vec2(1280.0, 720.0));
    for label in [
        "2 changed files",
        "src/main.rs",
        "image.png",
        "Modified",
        "+2",
        "−1",
        "    let message = \"old\";",
        "    let message = \"new\";",
        "Binary files a/image.png and b/image.png differ",
    ] {
        assert!(
            harness.query_all_by_label(label).next().is_some(),
            "missing {label}"
        );
    }
    let text = painted_texts(&harness);
    assert!(
        text.iter()
            .any(|(text, _)| text.contains("11") && text.ends_with('−'))
    );
    assert!(
        text.iter()
            .any(|(text, _)| text.contains("12") && text.ends_with('+'))
    );
    let row_fill = |needle: &str| {
        let position = text.iter().find(|(text, _)| text == needle).unwrap().1;
        harness
            .output()
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Rect(rect)
                    if rect.rect.contains(position + egui::vec2(1.0, 6.0))
                        && rect.rect.width() > 800.0
                        && (rect.rect.height() - 24.0).abs() < 0.5 =>
                {
                    Some(rect.fill)
                }
                _ => None,
            })
            .expect("code row has a full-width background")
    };
    let added = row_fill("    let message = \"new\";");
    let deleted = row_fill("    let message = \"old\";");
    assert_ne!(added, deleted);
    assert!(added.g() > added.r(), "added row is green");
    assert!(deleted.r() > deleted.g(), "deleted row is red");
}

#[test]
fn split_toggle_aligns_old_and_new_and_file_collapse_survives_view_changes() {
    use egui_kittest::kittest::Queryable;
    let mut harness = review_harness(egui::vec2(1280.0, 720.0));
    harness.get_by_label("Split").click();
    harness.run_steps(4);
    let text = painted_texts(&harness);
    let old = text
        .iter()
        .find(|(text, _)| text == "    let message = \"old\";")
        .unwrap()
        .1;
    let new = text
        .iter()
        .find(|(text, _)| text == "    let message = \"new\";")
        .unwrap()
        .1;
    assert!((old.y - new.y).abs() < 1.0);
    assert!(new.x - old.x > 400.0);
    harness.get_by_label("src/main.rs").click();
    harness.run_steps(4);
    assert!(
        harness
            .query_all_by_label("    let message = \"old\";")
            .next()
            .is_none()
    );
    assert!(
        harness
            .query_all_by_label("Binary files a/image.png and b/image.png differ")
            .next()
            .is_some()
    );
    harness.get_by_label("Unified").click();
    harness.run_steps(4);
    assert!(
        harness
            .query_all_by_label("    let message = \"old\";")
            .next()
            .is_none()
    );
    harness.get_by_label("src/main.rs").click();
    harness.run_steps(4);
    assert!(
        harness
            .query_all_by_label("    let message = \"old\";")
            .next()
            .is_some()
    );
}

#[test]
fn review_diff_virtualizes_large_documents_and_keeps_toolbar_inside_narrow_pane() {
    use egui_kittest::kittest::Queryable;
    let mut harness = review_harness(egui::vec2(480.0, 320.0));
    for label in [
        "Working tree",
        "Branch vs main",
        "Refresh",
        "Unified",
        "Split",
    ] {
        let rect = harness.get_by_label(label).rect();
        assert!(
            rect.min.x >= 0.0 && rect.max.x <= 480.0,
            "{label}: {rect:?}"
        );
    }
    let text = format!(
        "diff --git a/large.rs b/large.rs\n@@ -0,0 +1,5000 @@\n{}",
        (0..5000)
            .map(|i| format!("+let line_{i} = {i};\n"))
            .collect::<String>()
    );
    harness.state_mut().show_snapshot(text);
    harness.run_steps(4);
    assert!(
        harness
            .query_all_by_label("let line_0 = 0;")
            .next()
            .is_some()
    );
    assert!(
        painted_texts(&harness).len() < 100,
        "offscreen lines must not be painted"
    );
}

#[test]
#[ignore = "requires a working wgpu adapter; CI requires real diff render evidence"]
fn capture_review_diff_unified_and_split() {
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gui-evidence/diff-review");
    std::fs::create_dir_all(&directory).unwrap();
    for size in [[1280.0, 720.0], [800.0, 600.0]] {
        let temp = tempfile::tempdir().unwrap();
        let mut workbench = HeadlessWorkbench::new(
            state_with_diff(Arc::new(FixtureDiffSource::ready(REVIEW_DIFF)), temp.path()),
            size,
        );
        let path = workbench
            .state()
            .dock()
            .find_tab(&PanelId::new("diff-main"))
            .unwrap();
        workbench
            .state_mut()
            .dock_mut()
            .set_active_tab(path)
            .unwrap();
        workbench.run();
        workbench.click_label("Working tree");
        step_until(&mut workbench, "src/main.rs");
        workbench.run();
        for view in ["Unified", "Split"] {
            workbench.click_label(view);
            workbench.run();
            if let Some(frame) = gui::evidence::capture_or_skip(&mut workbench) {
                frame
                    .save_png(&directory.join(format!("{view}-{}x{}.png", size[0], size[1])))
                    .unwrap();
            }
        }
    }
}

#[test]
fn hidden_diff_tab_does_not_fetch_until_opened() {
    // Given: a selected project with another dock tab active.
    let temp = tempfile::tempdir().expect("temp dir");
    let source = Arc::new(RecordingDiffSource::default());
    let mut harness =
        HeadlessWorkbench::new(state_with_diff(source.clone(), temp.path()), [800.0, 600.0]);
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("subagents-home"))
        .expect("conversation tab");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("activate conversation");
    harness.run();
    // Then: no initial/periodic Git fetch is launched for the hidden diff tab.
    assert!(source.modes.lock().expect("modes lock").is_empty());
    // When: opening Diff, without touching Working tree or Refresh.
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("diff-main"))
        .expect("diff tab");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("activate diff");
    step_until(&mut harness, "branch body");
    assert_eq!(
        source.modes.lock().expect("modes lock").as_slice(),
        &[DiffMode::WorkingTree]
    );
}

#[test]
fn visible_diff_picks_up_real_git_edits_without_refresh_clicks() {
    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // Given: a clean repository and an open Diff tab.
    let temp = tempfile::tempdir().expect("temp dir");
    git(temp.path(), &["init", "-b", "main"]);
    std::fs::write(temp.path().join("tracked.txt"), "before\n").expect("fixture");
    git(temp.path(), &["add", "."]);
    git(
        temp.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "base",
        ],
    );
    let mut harness = diff_harness(Arc::new(gui::diff::GitCliDiffSource), temp.path());
    step_until(&mut harness, "no changes");
    // When: a tracked file changes on disk, and frames continue (no UI clicks).
    std::fs::write(temp.path().join("tracked.txt"), "after\n").expect("edit fixture");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        harness.step();
        if matches!(harness.state().diff().state(&DiffMode::WorkingTree), DiffState::Ready { text } if text.contains("+after"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "periodic diff never picked up the edit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Then: the changed file is rendered automatically.
    assert!(harness.has_label("tracked.txt"));
    assert!(harness.has_label("Auto-updating"));
}
