//! apply_patch tool cards: the patch reads as a diff, the result never does.

use egui::{Color32, FontFamily, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::model::transcript::TranscriptModel;
use gui::theme::tokens::palette;

const PATCH: &str = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** Update File: src/b.rs\n\
                     *** Move to: src/c.rs\n@@ fn main\n-old\n+new\n*** Delete File: d.txt\n\
                     *** End Patch\n";
const SUCCESS: &str = "Success. Updated the following files:\nA a.txt\nM src/c.rs\nD d.txt\n";
const FAILURE: &str = "apply_patch verification failed: Failed to find expected lines in \
                       src/b.rs:\n-old";

fn harness(is_error: bool) -> Harness<'static> {
    harness_sized(is_error, egui::vec2(800.0, 600.0))
}

fn harness_sized(is_error: bool, size: egui::Vec2) -> Harness<'static> {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "apply_patch".into(),
        call_id: "patch-1".into(),
        input: Some(serde_json::json!({"input": PATCH})),
        run_id: None,
        parent_call_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "apply_patch".into(),
        call_id: "patch-1".into(),
        is_error,
        output: Some(if is_error { FAILURE } else { SUCCESS }.into()),
        detail: None,
        run_id: None,
        parent_call_id: None,
    }));
    let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::agent::transcript_body(ui, &model);
    });
    harness.run_steps(2);
    harness
}

fn expand(harness: &mut Harness<'_>) {
    harness.get_by_label_contains("Apply patch a.txt").click();
    harness.run_steps(3);
}

fn text_shape(harness: &Harness<'_>, starts_with: &str) -> egui::epaint::TextShape {
    harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text().starts_with(starts_with) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no painted text starting with {starts_with:?}"))
}

/// The color `line` of `shape` is painted in. Neighboring lines of one color
/// share a section, so the section is found by position.
fn line_color(shape: &egui::epaint::TextShape, line: &str) -> Color32 {
    let job = &shape.galley.job;
    let start = job
        .text
        .find(line)
        .unwrap_or_else(|| panic!("{line:?} is not painted"));
    job.sections
        .iter()
        .find(|section| (section.byte_range.start.0..section.byte_range.end.0).contains(&start))
        .map(|section| section.format.color)
        .expect("every byte belongs to a section")
}

#[test]
fn apply_patch_card_names_the_first_file_and_counts_the_applied_ones() {
    // Given / When
    let harness = harness(false);
    // Then
    harness.get_by_label("✓ Apply patch a.txt +2 files · 3 files");
    assert!(harness.query_by_label("Patch").is_none());
}

#[test]
fn apply_patch_card_shows_the_patch_as_a_diff_and_the_result_as_code() {
    // Given
    let mut harness = harness(false);
    // When
    expand(&mut harness);
    // Then: the patch is labeled as such and colored by line.
    harness.get_by_label("Patch");
    harness.get_by_label("Output");
    assert!(harness.query_by_label("Input").is_none());
    assert!(harness.query_by_label("Diff").is_none());
    let patch = text_shape(&harness, "*** Begin Patch");
    assert_eq!(patch.galley.text(), PATCH);
    assert_eq!(line_color(&patch, "+hello\n"), palette().SUCCESS);
    assert_eq!(line_color(&patch, "-old\n"), palette().ERROR_FG);
    assert_eq!(line_color(&patch, "@@ fn main\n"), palette().INFO);
    assert_eq!(line_color(&patch, "*** Add File: a.txt\n"), palette().TEXT);
    // The result is one plain monospace block, not a diff.
    let output = text_shape(&harness, "Success. Updated");
    assert_eq!(output.galley.text(), SUCCESS);
    assert!(output.galley.job.sections.iter().all(|section| {
        section.format.color == palette().TEXT
            && section.format.font_id.family == FontFamily::Monospace
    }));
}

#[test]
fn apply_patch_card_capture_states_when_evidence_directory_is_set() {
    // Given
    let Some(directory) = std::env::var_os("EVORCH_TOOL_CAPTURE_DIR") else {
        return;
    };
    let directory = std::path::Path::new(&directory);
    for (size, width) in [
        (egui::vec2(800.0, 600.0), "wide"),
        (egui::vec2(320.0, 600.0), "narrow"),
    ] {
        for is_error in [false, true] {
            let mut harness = harness_sized(is_error, size);
            let kind = if is_error { "error" } else { "success" };
            // When / Then
            harness
                .render()
                .expect("render collapsed")
                .save(directory.join(format!("apply-patch-{kind}-{width}-collapsed.png")))
                .expect("save capture");
            expand(&mut harness);
            harness
                .render()
                .expect("render expanded")
                .save(directory.join(format!("apply-patch-{kind}-{width}-expanded.png")))
                .expect("save capture");
        }
    }
}

#[test]
fn failed_apply_patch_card_reports_an_error_without_a_file_count() {
    // Given
    let mut harness = harness(true);
    // When
    expand(&mut harness);
    // Then: no count, and the failure is not painted like an applied diff.
    harness.get_by_label("✗ Apply patch a.txt +2 files");
    harness.get_by_label("Patch");
    harness.get_by_label("Error");
    assert!(harness.query_by_label("Output").is_none());
    assert!(harness.query_by_label("Diff").is_none());
    let output = text_shape(&harness, "apply_patch verification failed");
    assert_eq!(output.galley.text(), FAILURE);
    assert!(
        output
            .galley
            .job
            .sections
            .iter()
            .all(|section| section.format.color == palette().ERROR_FG)
    );
}
