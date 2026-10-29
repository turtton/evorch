use std::sync::{Arc, Mutex};

use egui::load::ImageLoader as _;
use egui_kittest::Harness;
use event_bus::{ArtifactPresentation, Event, PresentedArtifact, ToolEvent};

use super::{set_transcript_virtualization_for_tests, transcript_layout_for_tests};
use crate::model::{html_snapshots::HtmlSnapshots, transcript::TranscriptModel};
use crate::panes::{agent::transcript_body, file_viewer};

type PreviewResult = Option<Result<Arc<egui::ColorImage>, String>>;

#[derive(Default)]
struct PreviewLoader(Mutex<PreviewResult>);

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
        if !uri.starts_with("evorch-preview://") {
            return Err(egui::load::LoadError::NotSupported);
        }
        match self.0.lock().unwrap().clone() {
            None => Ok(egui::load::ImagePoll::Pending { size: None }),
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

fn offscreen_preview_completion(html: bool, failed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(if html { "mock.html" } else { "mock.png" });
    std::fs::write(&path, "mock file; the loader never reads it").unwrap();
    let sha = "a".repeat(64);
    let mut model = TranscriptModel::new();
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
    for index in 0..2000 {
        model.push_notice(format!("History {index}"));
    }
    let image = egui::ColorImage::filled([320, 240], egui::Color32::BLUE);
    let loader = Arc::new(PreviewLoader::default());
    if html {
        *loader.0.lock().unwrap() = Some(Ok(Arc::new(image.clone())));
    }
    let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("snapshots")));
    let drawn_loader = Arc::clone(&loader);
    let drawn_snapshots = Arc::clone(&snapshots);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .with_wait_for_pending_images(false)
        .build_ui_state(
            move |ui, force| {
                crate::theme::install(ui.ctx());
                set_transcript_virtualization_for_tests(ui.ctx(), *force);
                // Override before the first load: no I/O thread or CEF is
                // involved in either side of the Pending transition.
                file_viewer::install_image_loader(ui.ctx());
                if !ui.ctx().is_loader_installed(drawn_loader.id()) {
                    ui.ctx().add_image_loader(drawn_loader.clone());
                }
                if html {
                    Arc::clone(&drawn_snapshots).install(ui.ctx());
                }
                transcript_body(ui, &model);
            },
            true,
        );
    harness.run();
    let pending = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert!(pending.1.y > 10_000.0, "preview is far above the viewport");
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
    assert!(completed.0.y > pending.0.y, "completion changes row height");

    // Compare before scrolling back to the preview. Visiting it first would
    // hide an incorrectly finalized Pending height.
    *harness.state_mut() = false;
    harness.run();
    let full = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert_eq!(
        (completed.0.y, completed.1.y),
        (full.0.y, full.1.y),
        "content extent and sticky offset after completion"
    );
}

#[test]
fn offscreen_image_pending_to_ready_and_failed_matches_full_layout() {
    for failed in [false, true] {
        offscreen_preview_completion(false, failed);
    }
}

#[test]
fn offscreen_html_pending_to_ready_and_failed_matches_full_layout() {
    for failed in [false, true] {
        offscreen_preview_completion(true, failed);
    }
}
