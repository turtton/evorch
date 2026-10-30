use std::sync::{Arc, Mutex};

use egui::load::ImageLoader as _;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use event_bus::{ArtifactPresentation, Event, MessageEvent, PresentedArtifact, ToolEvent};

use super::{set_transcript_virtualization_for_tests, transcript_layout_for_tests};
use crate::model::{html_snapshots::HtmlSnapshots, transcript::TranscriptModel};
use crate::panes::{agent::transcript_body, file_viewer};

type PreviewResult = Option<Result<Arc<egui::ColorImage>, String>>;

#[derive(Default)]
struct PreviewLoader(Mutex<PreviewResult>, Mutex<Option<egui::Vec2>>);

impl egui::load::ImageLoader for PreviewLoader {
    fn id(&self) -> &str {
        "transcript-preview-test-loader"
    }

    fn load(
        &self,
        _: &egui::Context,
        uri: &str,
        _: egui::load::SizeHint,
    ) -> egui::load::ImageLoadResult {
        if !uri.starts_with("evorch-preview://")
            && !uri.starts_with("mock://")
            && uri != "file://markdown.png"
        {
            return Err(egui::load::LoadError::NotSupported);
        }
        match self.0.lock().unwrap().clone() {
            None => Ok(egui::load::ImagePoll::Pending {
                size: *self.1.lock().unwrap(),
            }),
            Some(Ok(image)) => Ok(egui::load::ImagePoll::Ready { image }),
            Some(Err(error)) => Err(egui::load::LoadError::Loading(error)),
        }
    }

    fn forget(&self, _: &str) {}
    fn forget_all(&self) {}
    fn byte_size(&self) -> usize {
        0
    }
}

fn preview_harness(
    model: TranscriptModel,
    loader: Arc<PreviewLoader>,
    snapshots: Option<Arc<HtmlSnapshots>>,
) -> Harness<'static, bool> {
    Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .with_wait_for_pending_images(false)
        .build_ui_state(
            move |ui, force| {
                crate::theme::install(ui.ctx());
                set_transcript_virtualization_for_tests(ui.ctx(), *force);
                if !ui.ctx().is_loader_installed(loader.id()) {
                    // Install the production loaders first, then override them
                    // before any image load. Neither I/O threads nor CEF run.
                    crate::panes::markdown_render::render_markdown(ui, "", "install-loaders");
                    file_viewer::install_image_loader(ui.ctx());
                    ui.ctx().add_image_loader(loader.clone());
                }
                if let Some(snapshots) = &snapshots {
                    Arc::clone(snapshots).install(ui.ctx());
                }
                transcript_body(ui, &model);
            },
            true,
        )
}

#[derive(Clone, Copy)]
enum PreviewKind {
    Image,
    Html,
    Markdown { implicit: bool },
}

fn offscreen_preview_completion(kind: PreviewKind, failed: bool) {
    let html = matches!(kind, PreviewKind::Html);
    let markdown = matches!(kind, PreviewKind::Markdown { .. });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(if html { "mock.html" } else { "mock.png" });
    std::fs::write(&path, "mock file; the loader never reads it").unwrap();
    let sha = "a".repeat(64);
    let mut model = TranscriptModel::new();
    if markdown {
        let uri = if matches!(kind, PreviewKind::Markdown { implicit: true }) {
            "markdown.png"
        } else {
            "mock://markdown.png"
        };
        model.apply(&Event::new(MessageEvent::MessageDelta {
            run_id: Some("markdown-run".into()),
            delta: format!("Markdown image row\n\n![Mock markdown preview]({uri})"),
        }));
    } else {
        model.apply(&Event::new(ToolEvent::ArtifactsPresented {
            run_id: "preview-run".into(),
            presentation: ArtifactPresentation {
                presentation_id: "preview".into(),
                title: None,
                caption: None,
                artifacts: vec![PresentedArtifact {
                    artifact_id: "mock".into(),
                    title: "Mock preview".into(),
                    caption: None,
                    media_type: if html { "text/html" } else { "image/png" }.into(),
                    path: path.to_string_lossy().into_owned(),
                    byte_len: 1,
                    sha256: sha.clone(),
                }],
            },
        }));
    }
    for index in 0..2000 {
        model.push_notice(format!("History {index}"));
    }
    let image = egui::ColorImage::filled([320, 240], egui::Color32::BLUE);
    let loader = Arc::new(PreviewLoader(
        Mutex::default(),
        Mutex::new((markdown && failed).then_some(egui::vec2(320.0, 240.0))),
    ));
    if html {
        *loader.0.lock().unwrap() = Some(Ok(Arc::new(image.clone())));
    }
    let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("snapshots")));
    let mut harness = preview_harness(
        model,
        Arc::clone(&loader),
        html.then(|| Arc::clone(&snapshots)),
    );
    harness.run();
    harness.run_steps(3);
    let pending_label = match kind {
        PreviewKind::Html => "Rendering preview…",
        PreviewKind::Image => "Loading preview…",
        PreviewKind::Markdown { .. } => "Markdown image row",
    };
    assert!(
        harness.query_by_label(pending_label).is_none(),
        "stable Pending rows are not rendered offscreen"
    );
    let mut pending = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert!(pending.1.y > 10_000.0, "preview is far above the viewport");
    if markdown {
        *loader.1.lock().unwrap() = Some(egui::vec2(160.0, 60.0));
        harness.ctx.request_repaint();
        harness.run();
        harness.step();
        let metadata = transcript_layout_for_tests(&harness.ctx).unwrap();
        assert_ne!(
            metadata.0.y, pending.0.y,
            "Pending image dimensions changed"
        );
        assert!(harness.query_by_label(pending_label).is_none());
        pending = metadata;
    }
    let result = if failed {
        Err("mock preview failure\n".repeat(30))
    } else {
        Ok(image)
    };
    if html {
        assert_eq!(snapshots.take_requests().len(), 1);
        snapshots.finish(&sha, result);
    } else {
        *loader.0.lock().unwrap() = Some(result.map(Arc::new));
    }
    harness.ctx.request_repaint();
    harness.run();
    let completed = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert_ne!(completed.0.y, pending.0.y, "completion changes row height");

    // Compare before scrolling back to the preview. Visiting it first would
    // hide an incorrectly finalized Pending height.
    *harness.state_mut() = false;
    harness.run();
    let full = transcript_layout_for_tests(&harness.ctx).unwrap();
    if markdown {
        assert!(harness.query_by_label("Markdown image row").is_some());
    }
    assert_eq!(
        (completed.0.y, completed.1.y),
        (full.0.y, full.1.y),
        "content extent and sticky offset after completion"
    );
}

#[test]
fn offscreen_image_pending_to_ready_and_failed_matches_full_layout() {
    for failed in [false, true] {
        offscreen_preview_completion(PreviewKind::Image, failed);
    }
}

#[test]
fn offscreen_html_pending_to_ready_and_failed_matches_full_layout() {
    for failed in [false, true] {
        offscreen_preview_completion(PreviewKind::Html, failed);
    }
}

#[test]
fn offscreen_markdown_pending_to_ready_and_failed_matches_full_layout() {
    for failed in [false, true] {
        for implicit in [false, true] {
            offscreen_preview_completion(PreviewKind::Markdown { implicit }, failed);
        }
    }
}

fn add_artifact(model: &mut TranscriptModel, index: usize, path: &std::path::Path) {
    model.apply(&Event::new(ToolEvent::ArtifactsPresented {
        run_id: "pending-run".into(),
        presentation: ArtifactPresentation {
            presentation_id: format!("pending-{index}"),
            title: None,
            caption: None,
            artifacts: vec![PresentedArtifact {
                artifact_id: format!("artifact-{index}"),
                title: format!("Mock preview {index}"),
                caption: None,
                media_type: "image/png".into(),
                path: path.to_string_lossy().into_owned(),
                byte_len: 1,
                sha256: "a".repeat(64),
            }],
        },
    }));
}

#[test]
fn stable_pending_and_missing_previews_only_draw_the_viewport() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().join("pending.png");
    let missing = dir.path().join("missing.png");
    std::fs::write(&pending, "mock").unwrap();
    let mut model = TranscriptModel::new();
    for index in 0..1000 {
        add_artifact(
            &mut model,
            index,
            if index < 500 { &pending } else { &missing },
        );
    }
    let mut harness = preview_harness(model, Arc::default(), None);
    harness.run();
    for _ in 0..3 {
        harness.step();
        let count = harness.query_all_by_label_contains("Mock preview ").count();
        assert!(
            (1..=60).contains(&count),
            "only viewport cards are drawn: {count}"
        );
        assert!(
            harness.query_by_label("Loading preview…").is_none(),
            "all Pending image cards are far above the viewport"
        );
    }
    *harness.state_mut() = false;
    harness.run();
    assert!(
        harness.query_all_by_label_contains("Mock preview ").count() >= 1000,
        "the fixture contains all 1000 distinct cards"
    );
}

#[test]
fn offscreen_missing_preview_recovers_before_revisiting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovered.png");
    let mut model = TranscriptModel::new();
    add_artifact(&mut model, 0, &path);
    for index in 0..2000 {
        model.push_notice(format!("History {index}"));
    }
    let loader = Arc::new(PreviewLoader(
        Mutex::new(Some(Ok(Arc::new(egui::ColorImage::filled(
            [320, 240],
            egui::Color32::BLUE,
        ))))),
        Mutex::new(None),
    ));
    let mut harness = preview_harness(model, loader, None);
    harness.run();
    let missing = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert!(harness.query_by_label("Mock preview 0").is_none());
    std::fs::write(path, "now available").unwrap();
    // Availability's one-second retry deadline is a UI time contract, not a
    // wall-clock wait. Supply a frame beyond it explicitly.
    harness.input_mut().time = Some(harness.ctx.input(|input| input.time) + 2.0);
    harness.run();
    let recovered = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert!(recovered.0.y > missing.0.y);
    *harness.state_mut() = false;
    harness.run();
    let full = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert_eq!((recovered.0.y, recovered.1.y), (full.0.y, full.1.y));
}
