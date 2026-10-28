use super::*;
use egui_kittest::{Harness, kittest::Queryable};
use std::sync::Arc;

struct PreviewLoader(Result<Arc<egui::ColorImage>, String>);

impl egui::load::ImageLoader for PreviewLoader {
    fn id(&self) -> &str {
        "artifact-card-test-loader"
    }

    fn load(
        &self,
        _: &egui::Context,
        uri: &str,
        _: egui::load::SizeHint,
    ) -> egui::load::ImageLoadResult {
        if !uri.starts_with("evorch-preview://") {
            return Err(egui::load::LoadError::NotSupported);
        }
        self.0
            .clone()
            .map(|image| egui::load::ImagePoll::Ready { image })
            .map_err(egui::load::LoadError::Loading)
    }

    fn forget(&self, _: &str) {}
    fn forget_all(&self) {}
    fn byte_size(&self) -> usize {
        0
    }
}

// The card tests control loader completion instead of racing the I/O thread.
fn install_preview_loader(ctx: &egui::Context, result: Result<egui::ColorImage, String>) {
    crate::panes::file_viewer::install_image_loader(ctx);
    let loader = PreviewLoader(result.map(Arc::new));
    if !ctx.is_loader_installed(egui::load::ImageLoader::id(&loader)) {
        ctx.add_image_loader(Arc::new(loader));
    }
}

fn ready_preview(ctx: &egui::Context) {
    install_preview_loader(
        ctx,
        Ok(egui::ColorImage::filled([4, 3], egui::Color32::BLUE)),
    );
}

fn artifact(id: &str, title: &str, media_type: &str, path: &Path) -> PresentedArtifact {
    PresentedArtifact {
        artifact_id: id.into(),
        title: title.into(),
        caption: None,
        media_type: media_type.into(),
        path: path.to_string_lossy().into_owned(),
        byte_len: 1,
        sha256: "0".repeat(64),
    }
}

#[test]
fn card_opens_stored_artifacts_through_the_host_and_marks_missing_ones() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("mock.png");
    image::RgbImage::new(4, 3).save(&image).unwrap();
    let html = dir.path().join("mock.html");
    std::fs::write(&html, "<p>mock</p>").unwrap();
    let presentation = ArtifactPresentation {
        presentation_id: "presentation-1".into(),
        title: Some("Login variants".into()),
        caption: Some("Compare the layouts".into()),
        artifacts: vec![
            artifact("artifact-a", "Variant A", "image/png", &image),
            artifact("artifact-b", "Variant B", "text/html", &html),
            artifact(
                "artifact-gone",
                "Variant C",
                "image/png",
                &dir.path().join("deleted.png"),
            ),
        ],
    };
    let shown = presentation.clone();
    let mut harness = Harness::new_ui(move |ui| {
        ready_preview(ui.ctx());
        show(ui, egui::Id::new("card"), &shown);
    });
    harness.run();

    for label in ["Login variants", "Compare the layouts"] {
        assert!(harness.query_by_label_contains(label).is_some(), "{label}");
    }
    assert!(
        harness
            .query_by_label_contains("Artifact unavailable: artifact-gone")
            .is_some()
    );
    // Open stays visible for every card, including the missing file.
    let open = harness.query_all_by_label("Open").collect::<Vec<_>>();
    assert_eq!(open.len(), 3);
    open[0].click();
    harness.run();
    assert_eq!(take_external_open_requests(&harness.ctx), vec![image]);
    harness.get_all_by_label("Open").nth(1).unwrap().click();
    harness.run();
    assert_eq!(take_external_open_requests(&harness.ctx), vec![html]);
    assert!(take_external_open_requests(&harness.ctx).is_empty());
    assert!(take_open_requests(&harness.ctx).is_empty());
}

#[test]
fn summary_names_every_artifact() {
    let path = Path::new("/missing");
    let mut presentation = ArtifactPresentation {
        presentation_id: "p".into(),
        title: None,
        caption: None,
        artifacts: vec![
            artifact("a", "One", "image/png", path),
            artifact("b", "Two", "text/html", path),
        ],
    };
    assert_eq!(summary(&presentation), "Artifacts: One, Two");
    presentation.title = Some("Mocks".into());
    assert_eq!(summary(&presentation), "Artifacts: Mocks (One, Two)");
}

#[test]
fn html_cards_show_the_rendered_snapshot_and_open_on_click() {
    let dir = tempfile::tempdir().unwrap();
    let html = dir.path().join("mock.html");
    std::fs::write(&html, "<p>mock</p>").unwrap();
    let mut mock = artifact("artifact-b", "Variant B", "text/html", &html);
    mock.sha256 = "ab".repeat(32);
    let presentation = ArtifactPresentation {
        presentation_id: "presentation-1".into(),
        title: None,
        caption: None,
        artifacts: vec![mock.clone()],
    };
    let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("snapshots")));
    let shown = presentation.clone();
    let mut harness = Harness::new_ui(move |ui| {
        ready_preview(ui.ctx());
        show(ui, egui::Id::new("card"), &shown);
    });
    Arc::clone(&snapshots).install(&harness.ctx);
    harness.step();

    assert!(
        harness
            .query_by_label_contains("Rendering preview")
            .is_some()
    );
    let requests = snapshots.take_requests();
    assert_eq!(requests.len(), 1);
    snapshots.finish(
        &mock.sha256,
        Ok(egui::ColorImage::filled([4, 3], egui::Color32::BLUE)),
    );
    harness.step();

    let preview = harness.get_by_label("Variant B preview");
    assert!(preview.rect().width() > 0.0 && preview.rect().height() > 0.0);
    harness.get_by_label("Variant B preview").click();
    harness.step();
    assert_eq!(take_open_requests(&harness.ctx), vec![html]);
}

#[test]
fn html_cards_without_a_snapshot_renderer_explain_why_and_can_open_externally() {
    let dir = tempfile::tempdir().unwrap();
    let html = dir.path().join("mock.html");
    std::fs::write(&html, "<p>mock</p>").unwrap();
    let presentation = ArtifactPresentation {
        presentation_id: "p".into(),
        title: None,
        caption: None,
        artifacts: vec![artifact("a", "One", "text/html", &html)],
    };
    let mut harness = Harness::new_ui(move |ui| show(ui, egui::Id::new("card"), &presentation));
    harness.run();

    assert!(
        harness
            .query_by_label_contains("HTML renderer (CEF) is not installed")
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("Rendering preview")
            .is_none()
    );
    harness.get_by_label("Open").click();
    harness.run();
    assert_eq!(
        take_external_open_requests(&harness.ctx),
        vec![dir.path().join("mock.html")]
    );
}

#[test]
fn a_missing_artifact_is_rechecked_and_shown_when_the_store_reappears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.png");
    let shown = artifact("late", "Restored", "image/png", &path);
    let mut harness = Harness::new_ui(move |ui| {
        ready_preview(ui.ctx());
        artifact_item(ui, &shown);
    });
    harness.run();
    assert!(
        harness
            .query_by_label_contains("Artifact unavailable")
            .is_some()
    );
    assert!(harness.query_by_label("Open").is_some());
    image::RgbImage::new(4, 3).save(&path).unwrap();
    harness.step();
    assert!(
        harness
            .query_by_label_contains("Artifact unavailable")
            .is_some()
    );
    let now = harness.ctx.input(|input| input.time);
    harness.input_mut().time = Some(now + AVAILABILITY_RECHECK_INTERVAL.as_secs_f64());
    harness.run();
    assert!(
        harness
            .query_by_label_contains("Artifact unavailable")
            .is_none()
    );
    assert!(harness.query_by_label("Restored preview").is_some());
    harness.get_by_label("Open").click();
    harness.run();
    assert_eq!(take_external_open_requests(&harness.ctx), vec![path]);
}

#[test]
fn missing_artifact_checks_are_throttled_and_detect_recovery_at_the_deadline() {
    let ctx = egui::Context::default();
    let path = Path::new("/artifacts/recovering.png");
    let checks = std::cell::Cell::new(0);
    let present = std::cell::Cell::new(false);
    let check = |checked_path: &Path| {
        assert_eq!(checked_path, path);
        checks.set(checks.get() + 1);
        present.get()
    };
    let interval = AVAILABILITY_RECHECK_INTERVAL.as_secs_f64();
    assert!(!is_available_at(&ctx, path, 0.0, check));
    for now in [0.0, 0.0, interval / 2.0, interval - 0.001] {
        assert!(!is_available_at(&ctx, path, now, check));
    }
    assert_eq!(checks.get(), 1, "frames before the deadline reuse the miss");
    assert!(!is_available_at(&ctx, path, interval, check));
    assert_eq!(checks.get(), 2, "an expired miss gets one new check");
    present.set(true);
    for now in [interval, 1.5 * interval, 2.0 * interval - 0.001] {
        assert!(!is_available_at(&ctx, path, now, check));
    }
    assert_eq!(checks.get(), 2, "a repeated miss renews the deadline");
    assert!(is_available_at(&ctx, path, 2.0 * interval, check));
    assert_eq!(checks.get(), 3, "the exact deadline detects recovery");
    assert!(is_available_at(&ctx, path, 10.0 * interval, check));
    assert_eq!(checks.get(), 3, "immutable available files remain cached");
}

#[test]
fn long_titles_do_not_push_open_outside_a_narrow_card() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mock.html");
    std::fs::write(&path, "<p>mock</p>").unwrap();
    let shown = artifact("long", &"A long mock title ".repeat(20), "text/html", &path);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(260.0, 160.0))
        .build_ui(move |ui| artifact_item(ui, &shown));
    harness.run();
    let button = harness.get_by_label("Open");
    let rect = button.rect();
    assert!(rect.width() > 0.0 && rect.min.x >= 0.0 && rect.max.x <= 260.0);
    button.click();
    harness.run();
    assert_eq!(take_external_open_requests(&harness.ctx), vec![path]);
}

#[test]
fn png_loaded_from_disk_keeps_its_size_below_the_scroll_viewport() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mock with spaces.png");
    image::RgbImage::from_pixel(854, 480, image::Rgb([20, 80, 160]))
        .save(&path)
        .unwrap();
    let shown = artifact("png", "Stored PNG", "image/png", &path);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(360.0, 180.0))
        .build_ui(move |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(300.0);
                artifact_item(ui, &shown);
            });
        });
    crate::panes::file_viewer::tests::wait_for_image(&harness.ctx, &path);
    harness.run();
    let size = harness.get_by_label("Stored PNG preview").rect().size();
    assert!(size.x > 0.0 && size.y > 0.0, "{size:?}");
    assert!(size.x <= 360.0 && size.y <= PREVIEW_MAX_HEIGHT);
    assert!((size.x / size.y - 854.0 / 480.0).abs() < 0.01);
    assert!(
        harness
            .query_by_label_contains("Preview unavailable")
            .is_none()
    );
}

#[test]
fn portrait_and_wide_previews_fit_cards_at_normal_and_high_dpi() {
    let dir = tempfile::tempdir().unwrap();
    for (width, height) in [(300, 1800), (1800, 675)] {
        let path = dir.path().join(format!("{width}x{height}.png"));
        image::RgbImage::from_pixel(width, height, image::Rgb([20, 80, 160]))
            .save(&path)
            .unwrap();
        for card_width in [360.0, 900.0] {
            for dpi in [1.0, 2.0] {
                let shown = ArtifactPresentation {
                    presentation_id: "sizing".into(),
                    title: None,
                    caption: None,
                    artifacts: vec![artifact("png", "Sized PNG", "image/png", &path)],
                };
                let mut harness = Harness::builder()
                    .with_size(egui::vec2(card_width, 700.0))
                    .with_pixels_per_point(dpi)
                    .build_ui_state(
                        move |ui, card_rect| {
                            *card_rect = ui
                                .scope(|ui| show(ui, egui::Id::new("card"), &shown))
                                .response
                                .rect;
                        },
                        egui::Rect::NOTHING,
                    );
                crate::panes::file_viewer::tests::wait_for_image(&harness.ctx, &path);
                harness.run();
                assert_eq!(harness.ctx.pixels_per_point(), dpi);
                let rect = harness.get_by_label("Sized PNG preview").rect();
                let size = rect.size();
                let bounds = harness.ctx.content_rect();
                assert!(size.x > 0.0 && size.y > 0.0);
                assert!(harness.state().contains_rect(rect));
                assert!(rect.min.x >= bounds.min.x && rect.max.x <= bounds.max.x);
                assert!(size.x <= PREVIEW_MAX_WIDTH && size.y <= PREVIEW_MAX_HEIGHT);
                assert!((size.x / size.y - width as f32 / height as f32).abs() < 0.01);
                if height > width {
                    assert!((size.y - PREVIEW_MAX_HEIGHT).abs() < 0.01);
                } else if card_width > PREVIEW_MAX_WIDTH {
                    assert!((size.x - PREVIEW_MAX_WIDTH).abs() < 0.01);
                }
            }
        }
    }
}

#[test]
fn image_load_failures_are_visible_without_hover_and_keep_open_available() {
    for reason in [
        "Cannot read image: corrupt PNG",
        "Files larger than 32 MiB cannot be previewed.",
        "Images are previewed up to 8192 px per side and 16 megapixels in total.",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.png");
        std::fs::write(&path, b"invalid").unwrap();
        let shown = artifact("bad", "Failed", "image/png", &path);
        let mut harness = Harness::new_ui(move |ui| {
            install_preview_loader(ui.ctx(), Err(reason.into()));
            artifact_item(ui, &shown);
        });
        harness.run();
        assert!(
            harness.query_by_label_contains(reason).is_some(),
            "{reason}"
        );
        assert!(harness.query_by_label("Failed preview").is_none());
        harness.get_by_label("Open").click();
        harness.run();
        assert_eq!(take_external_open_requests(&harness.ctx), vec![path]);
    }
}

#[test]
fn html_render_failures_and_unknown_media_types_keep_an_external_open_action() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mock.html");
    std::fs::write(&path, "<p>mock</p>").unwrap();
    for (media_type, reason) in [
        ("text/html", "CEF initialization failed"),
        ("application/octet-stream", "unsupported media type"),
    ] {
        let shown = artifact("mock", "Mock", media_type, &path);
        let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("snapshots")));
        snapshots.finish(&shown.sha256, Err("CEF initialization failed".into()));
        let mut harness = Harness::new_ui(move |ui| {
            snapshots.clone().install(ui.ctx());
            artifact_item(ui, &shown);
        });
        harness.run();
        assert!(harness.query_by_label_contains(reason).is_some());
        harness.get_by_label("Open").click();
        harness.run();
        assert_eq!(
            take_external_open_requests(&harness.ctx),
            vec![path.clone()]
        );
    }
}
