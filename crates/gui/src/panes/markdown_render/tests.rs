use egui::{FontFamily, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};

use super::render_markdown;
use crate::theme::tokens::{ACCENT_FG, SURFACE_RAISED};

fn harness(source: &'static str) -> Harness<'static> {
    let mut harness = Harness::new_ui(move |ui| {
        crate::theme::install(ui.ctx());
        render_markdown(ui, source, "test-message");
    });
    harness.run_steps(2);
    harness
}

fn text_shape(harness: &Harness<'_>, text: &str) -> egui::epaint::TextShape {
    harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Text(shape) if shape.galley.text().contains(text) => Some(shape.clone()),
            _ => None,
        })
        .expect("rendered text")
}

#[test]
fn render_markdown_bold_text_uses_strong_color() {
    // Given / When
    let harness = harness("**important**");
    // Then
    let shape = text_shape(&harness, "important");
    assert_eq!(shape.galley.text(), "important");
    assert!(
        shape
            .galley
            .job
            .sections
            .iter()
            .all(|s| s.format.color == ACCENT_FG)
    );
}

#[test]
fn render_markdown_list_items_indented() {
    // Given / When
    let harness = harness("intro\n\n- first\n- second\n\n1. numbered");
    // Then
    let intro = harness.get_by_label("intro").rect();
    let first = harness.get_by_label("first").rect();
    let second = harness.get_by_label("second").rect();
    assert!(first.left() > intro.left());
    assert_eq!(first.left(), second.left());
    assert!(second.top() > first.top());
    assert!(harness.get_by_label("numbered").rect().left() > intro.left());
}

#[test]
fn render_markdown_code_block_has_background() {
    // Given / When
    let harness = harness("```\nlet x = 1;\n```");
    // Then
    let code = text_shape(&harness, "let x");
    assert!(!code.galley.text().contains("```"));
    assert!(harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == SURFACE_RAISED
            && rect.rect.contains(code.pos))
    }));
}

#[test]
fn render_markdown_inline_code_uses_monospace() {
    // Given / When
    let harness = harness("`value()`");
    // Then
    let shape = text_shape(&harness, "value()");
    assert_eq!(shape.galley.text(), "value()");
    assert!(
        shape
            .galley
            .job
            .sections
            .iter()
            .all(|s| s.format.font_id.family == FontFamily::Monospace)
    );
}

#[test]
fn render_markdown_soft_break_handling() {
    // Given / When: CommonMark soft breaks become spaces, hard breaks remain lines.
    let harness = harness("first\nsecond  \nthird");
    // Then
    let first = text_shape(&harness, "first");
    let second = text_shape(&harness, "second");
    assert_eq!(first.pos.y, second.pos.y);
    assert!(
        second.pos.x + second.galley.mesh_bounds.left()
            > first.pos.x + first.galley.mesh_bounds.right()
    );
    let third = text_shape(&harness, "third");
    assert!(third.pos.y > first.pos.y || third.galley.rows.len() > 1);
}

#[test]
fn transcript_message_renders_markdown() {
    // Given
    let mut model = crate::model::transcript::TranscriptModel::new();
    model.push_message("**agent answer**");
    model.push_user_message("**user input**");
    model.push_reasoning("**thinking**");
    // When
    let mut harness = Harness::new_ui(move |ui| {
        crate::theme::install(ui.ctx());
        crate::panes::agent::transcript_body(ui, &model);
    });
    harness.run_steps(2);
    // Then
    assert!(harness.query_by_label("agent answer").is_some());
    assert!(harness.query_by_label("You: **user input**").is_some());
    assert!(harness.query_by_label("Reasoning: **thinking**").is_some());
}

#[test]
fn render_markdown_headings_emphasis_and_links() {
    // Given / When
    let harness = harness(
        "# Heading\n\n## Subheading\n\n### Detail\n\n*italic* ~~deleted~~\n\n[website](https://example.com)\n\n> quoted",
    );
    // Then
    let heading = text_shape(&harness, "Heading");
    let subheading = text_shape(&harness, "Subheading");
    let detail = text_shape(&harness, "Detail");
    assert!(
        heading.galley.job.sections[0].format.font_id.size
            > subheading.galley.job.sections[0].format.font_id.size
    );
    assert!(
        subheading.galley.job.sections[0].format.font_id.size
            > detail.galley.job.sections[0].format.font_id.size
    );
    assert!(
        text_shape(&harness, "italic")
            .galley
            .job
            .sections
            .iter()
            .any(|s| s.format.italics)
    );
    assert!(
        text_shape(&harness, "deleted")
            .galley
            .job
            .sections
            .iter()
            .any(|s| s.format.strikethrough.width > 0.0)
    );
    assert!(harness.query_by_label("website").is_some());
    assert_eq!(
        text_shape(&harness, "website").fallback_color,
        crate::theme::tokens::ACCENT
    );
    assert!(harness.query_by_label("quoted").is_some());
}
