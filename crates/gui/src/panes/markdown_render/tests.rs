use egui::{FontFamily, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};

use super::render_markdown;
use crate::theme::tokens::palette;

mod nested;

fn assert_narrow_content(source: String, prose: bool) {
    // Given: a narrow message area inside a wider viewport, as in a dock pane.
    let mut harness = Harness::builder()
        .with_size(egui::vec2(600.0, 1200.0))
        .build_ui(move |ui| {
            crate::theme::install(ui.ctx());
            ui.set_width(200.0);
            let right = ui.max_rect().right();
            // When: markdown is laid out at the message width.
            render_markdown(ui, &source, "narrow-message");
            assert!(ui.min_rect().right() <= right + 1.0, "layout overflow");
        });
    harness.run_steps(2);
    // Then: visible ink fits, and prose really wraps rather than being clipped.
    let mut rows = 0;
    for clipped in &harness.output().shapes {
        if let Shape::Text(text) = &clipped.shape {
            rows += text.galley.rows.len();
            let ink = text.galley.mesh_bounds.translate(text.pos.to_vec2());
            let bounds = if prose {
                ink
            } else {
                ink.intersect(clipped.clip_rect)
            };
            assert!(bounds.right() <= 209.0, "paint overflow: {bounds:?}");
        }
    }
    assert!(rows > 0, "must paint content");
    if prose {
        assert!(rows > 1, "prose must wrap");
    }
}

#[test]
fn narrow_cjk_paragraph_wraps() {
    assert_narrow_content("日本語の長い会話本文を表示します".repeat(12), true);
}

#[test]
fn narrow_unbroken_latin_wraps() {
    assert_narrow_content("abcdefghijklmno".repeat(20), true);
}

#[test]
fn narrow_fenced_code_stays_within_message() {
    assert_narrow_content(
        format!("```\n    {}\n```", "long_code_token".repeat(20)),
        false,
    );
}

#[test]
fn narrow_code_preserves_indentation_and_line_structure() {
    // Given: an indented code line wider than the message.
    let line = format!("    {}", "long_code_token".repeat(20));
    let source = format!("```\n{line}\n    next_line\n```");
    // When: rendering in a narrow message area.
    let mut harness = Harness::new_ui(move |ui| {
        ui.set_width(200.0);
        render_markdown(ui, &source, "code-lines");
    });
    harness.run_steps(2);
    // Then: code keeps its two logical lines and can extend inside its scroller.
    let code = text_shape(&harness, "long_code_token");
    assert_eq!(code.galley.text(), format!("{line}\n    next_line"));
    assert_eq!(code.galley.rows.len(), 2);
    assert!(code.galley.rect.width() > 200.0);
}

#[test]
fn transcript_prose_after_wide_table_wraps_inside_card() {
    // Given: a wide table followed by ordinary prose in the real transcript.
    let mut model = crate::model::transcript::TranscriptModel::new();
    model.push_message(format!(
        "| First | Second |\n| --- | --- |\n| {} | {} |",
        "wide".repeat(30),
        "cell".repeat(30)
    ));
    model.push_message("abcdefghijklmno".repeat(20));
    // When: drawing the conversation in a 200px pane.
    let mut harness = Harness::builder()
        .with_size(egui::vec2(600.0, 1200.0))
        .build_ui(move |ui| {
            crate::theme::install(ui.ctx());
            ui.set_width(200.0);
            crate::panes::agent::transcript_body(ui, &model);
        });
    harness.run_steps(2);
    // Then: prose wraps inside the card, not merely the viewport clip rectangle.
    let prose = text_shape(&harness, "abcdefghijklmno");
    assert!(prose.galley.rows.len() > 1);
    assert!(prose.pos.x + prose.galley.mesh_bounds.right() <= 196.0);
}

#[test]
fn narrow_table_stays_within_message() {
    assert_narrow_content(
        format!(
            "| First | Second |\n| --- | --- |\n| {} | {} |",
            "wide".repeat(30),
            "cell".repeat(30)
        ),
        false,
    );
}

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
            .all(|s| s.format.color == palette().TEXT)
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
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == palette().SURFACE_RAISED
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
    assert!(harness.query_by_label("thinking").is_some());
    assert!(harness.query_by_label("**thinking**").is_none());
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
        crate::theme::tokens::palette().ACCENT
    );
    assert!(harness.query_by_label("quoted").is_some());
}
