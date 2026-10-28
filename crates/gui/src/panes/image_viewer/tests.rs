use super::*;
use crate::panes::artifact_card::{self, take_open_requests};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{ArtifactPresentation, PresentedArtifact};
use std::path::Path;

fn presentation(path: &Path) -> ArtifactPresentation {
    ArtifactPresentation {
        presentation_id: "presentation-1".into(),
        title: None,
        caption: None,
        artifacts: vec![PresentedArtifact {
            artifact_id: "artifact-a".into(),
            title: "Variant A".into(),
            caption: None,
            media_type: "image/png".into(),
            path: path.to_string_lossy().into_owned(),
            byte_len: 1,
            sha256: "0".repeat(64),
        }],
    }
}

fn card_with_viewer(path: &Path) -> Harness<'static> {
    let shown = presentation(path);
    let harness = Harness::builder()
        .with_size(egui::vec2(900.0, 700.0))
        .build_ui(move |ui| {
            artifact_card::show(ui, egui::Id::new("card"), &shown);
            show(ui.ctx());
        });
    crate::panes::file_viewer::tests::wait_for_image(&harness.ctx, path);
    harness
}

fn step_until(harness: &mut Harness<'_>, label: &str) {
    harness.run();
    assert!(harness.query_by_label_contains(label).is_some(), "{label}");
}

fn image(dir: &Path, width: u32, height: u32) -> PathBuf {
    let path = dir.join(format!("{width}x{height}.png"));
    image::RgbImage::new(width, height).save(&path).unwrap();
    path
}

#[test]
fn preview_click_opens_the_viewer_and_its_controls_zoom() {
    let dir = tempfile::tempdir().unwrap();
    let path = image(dir.path(), 400, 300);
    let mut harness = card_with_viewer(&path);
    step_until(&mut harness, "Variant A preview");

    assert_eq!(harness.query_all_by_label("Open").count(), 1);
    harness.get_by_label("Variant A preview").click();
    step_until(&mut harness, "Variant A (Fit · 100%)");
    assert!(is_open(&harness.ctx));

    harness.get_by_label("Zoom in").click();
    step_until(&mut harness, "Variant A (125%)");
    harness.get_by_label("Fit to window").click();
    step_until(&mut harness, "Variant A (Fit · 100%)");
    harness.get_by_label("Zoom out").click();
    step_until(&mut harness, "Variant A (80%)");
    harness.get_by_label("Actual size").click();
    step_until(&mut harness, "Variant A (100%)");

    harness.get_by_label("Close image viewer").click();
    harness.step();
    assert!(!is_open(&harness.ctx));
    assert!(take_open_requests(&harness.ctx).is_empty());
}

#[test]
fn large_images_fit_the_window_and_escape_closes() {
    let dir = tempfile::tempdir().unwrap();
    let path = image(dir.path(), 2000, 1500);
    let mut harness = card_with_viewer(&path);
    step_until(&mut harness, "Variant A preview");
    harness.get_by_label("Variant A preview").click();
    step_until(&mut harness, "Variant A (Fit");
    assert!(
        harness
            .query_by_label_contains("Variant A (Fit · 100%)")
            .is_none()
    );

    harness.key_press(egui::Key::Num1);
    step_until(&mut harness, "Variant A (100%)");
    harness.key_press(egui::Key::Escape);
    harness.step();
    assert!(!is_open(&harness.ctx));
}

#[test]
fn context_menus_open_the_image_in_another_app() {
    let dir = tempfile::tempdir().unwrap();
    let path = image(dir.path(), 40, 30);
    let mut harness = card_with_viewer(&path);
    step_until(&mut harness, "Variant A preview");

    harness.get_by_label("Variant A preview").click_secondary();
    step_until(&mut harness, OPEN_EXTERNALLY);
    harness.get_by_label(OPEN_EXTERNALLY).click();
    harness.step();
    assert_eq!(take_open_requests(&harness.ctx), vec![path.clone()]);
    assert!(!is_open(&harness.ctx));

    harness.get_by_label("Variant A preview").click();
    step_until(&mut harness, "Variant A (Fit");
    // A press moves the modal to the top. Pressing and releasing in separate
    // frames, as a person does, opens the menu above it.
    let center = harness
        .get_by_label_contains("Variant A (Fit")
        .rect()
        .center();
    harness.hover_at(center);
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton {
            pos: center,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();
    }
    step_until(&mut harness, OPEN_EXTERNALLY);
    harness.get_by_label(OPEN_EXTERNALLY).click();
    harness.step();
    assert_eq!(take_open_requests(&harness.ctx), vec![path]);
}

#[test]
fn fit_never_enlarges_and_counts_physical_pixels() {
    let viewport = egui::vec2(50.0, 50.0);
    assert_eq!(fit_zoom(egui::vec2(10.0, 10.0), viewport, 1.0), 1.0);
    // 200x100 px is 100x50 points at 2x, so half of it fits.
    assert_eq!(fit_zoom(egui::vec2(200.0, 100.0), viewport, 2.0), 0.5);
    assert_eq!(fit_zoom(egui::vec2(0.0, 10.0), viewport, 1.0), 1.0);
}

#[test]
fn zooming_keeps_the_point_under_the_pointer() {
    let image_px = egui::vec2(1000.0, 800.0);
    let viewport = egui::vec2(400.0, 300.0);
    let start = View::Free {
        zoom: 1.0,
        pan: egui::vec2(30.0, -20.0),
    };
    let anchor = egui::vec2(100.0, 50.0);
    // Image coordinate under the anchor, relative to the image center.
    let under = |view: View| {
        let (zoom, pan) = view.resolve(image_px, viewport, 1.0);
        (anchor - pan) / zoom
    };
    let zoomed = start.zoom_by(2.0, anchor, image_px, viewport, 1.0);
    let View::Free { zoom, .. } = zoomed else {
        panic!("zooming leaves fit mode");
    };
    assert_eq!(zoom, 2.0);
    assert!((under(start) - under(zoomed)).length() < 1e-3);

    let clamped = start.zoom_by(1e6, anchor, image_px, viewport, 1.0);
    assert!(matches!(clamped, View::Free { zoom, .. } if zoom == MAX_ZOOM));
}

#[test]
fn panning_keeps_part_of_the_image_visible() {
    let image_px = egui::vec2(100.0, 100.0);
    let viewport = egui::vec2(400.0, 300.0);
    let panned = View::Fit.pan_by(egui::vec2(10_000.0, -10_000.0), image_px, viewport, 1.0);
    let View::Free { pan, zoom } = panned else {
        panic!("panning leaves fit mode");
    };
    assert_eq!(zoom, 1.0);
    assert_eq!(pan, egui::vec2(250.0 - PAN_MARGIN, -(200.0 - PAN_MARGIN)));
}
