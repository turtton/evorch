use super::*;

fn nested_harness(source: String) -> Harness<'static> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(600.0, 1200.0))
        .build_ui(move |ui| {
            crate::theme::install(ui.ctx());
            ui.set_width(200.0);
            let right = ui.max_rect().right();
            render_markdown(ui, &source, "nested");
            assert!(
                ui.min_rect().right() <= right + 1.0,
                "layout {:?} right {right}",
                ui.min_rect()
            );
        });
    harness.run_steps(2);
    harness
}

fn assert_local_scroll(harness: &mut Harness<'_>, wide: &str) {
    let before = text_shape(harness, "before");
    let after = text_shape(harness, "after");
    for prose in [&before, &after] {
        assert!(prose.galley.rows.len() > 1, "prose must wrap");
        let ink = prose.galley.mesh_bounds.translate(prose.pos.to_vec2());
        assert!(
            ink.left() >= 8.0 && ink.right() <= 209.0,
            "unclipped prose: {ink:?}"
        );
    }
    let clipped = harness
        .output()
        .shapes
        .iter()
        .find(
            |shape| matches!(&shape.shape, Shape::Text(text) if text.galley.text().contains(wide)),
        )
        .expect("wide content");
    let Shape::Text(text) = &clipped.shape else {
        unreachable!()
    };
    assert!(text.galley.rect.width() > 200.0, "natural width retained");
    assert!(
        clipped.clip_rect.width() <= 201.0,
        "local horizontal viewport"
    );
    assert!(clipped.clip_rect.right() <= 209.0);
    assert!(text.pos.y >= before.pos.y + before.galley.rect.bottom());
    assert!(text.pos.y + text.galley.rect.bottom() <= after.pos.y);
    for marker in ["before", "after"] {
        let prose = harness.output().shapes.iter().find(|shape| {
            matches!(&shape.shape, Shape::Text(text) if text.galley.text().contains(marker))
        }).expect("surrounding prose");
        assert!(
            prose.clip_rect.width() > clipped.clip_rect.width(),
            "prose outside local scroller"
        );
    }
    assert!(text.pos.x > 8.0, "nested indentation retained");
    let position = text.pos;
    harness.event(egui::Event::PointerMoved(position + egui::vec2(20.0, 5.0)));
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(-100.0, 0.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(2);
    assert!(
        text_shape(harness, wide).pos.x < position.x,
        "content scrolls horizontally"
    );
    assert_eq!(text_shape(harness, "before").pos, before.pos);
    assert_eq!(text_shape(harness, "after").pos, after.pos);
}

#[test]
fn nested_blockquote_table_keeps_surrounding_prose_wrapped() {
    // Given: prose and a wide table share the same blockquote.
    let source = format!(
        "> before{}\n>\n> | First | Second |\n> | --- | --- |\n> | {} | value |\n>\n> after{}",
        "日本語の長い本文".repeat(10),
        "wide_cell".repeat(30),
        "abcdefghijklmno".repeat(10)
    );
    // When: rendering the message at 200px.
    let mut harness = nested_harness(source);
    // Then: only the table has a wide galley inside a local clip region.
    assert_local_scroll(&mut harness, "wide_cell");
}

#[test]
fn nested_list_code_keeps_surrounding_prose_wrapped() {
    // Given: prose and a wide fenced block share the same list item.
    let line = format!("    {}", "wide_code".repeat(30));
    let source = format!(
        "- before{}\n\n  ```\n  {line}\n      next_line\n  ```\n\n  after{}",
        "abcdefghijklmno".repeat(10),
        "日本語の長い本文".repeat(10)
    );
    // When: rendering the message at 200px.
    let mut harness = nested_harness(source);
    // Then: prose wraps, while code preserves indentation and logical lines.
    assert_local_scroll(&mut harness, "wide_code");
    let code = text_shape(&harness, "wide_code");
    assert_eq!(code.galley.text(), format!("{line}\n    next_line"));
    assert_eq!(code.galley.rows.len(), 2);
}
