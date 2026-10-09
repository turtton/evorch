use super::*;
use egui_kittest::{Harness, kittest::Queryable};

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
    let mut harness = Harness::new_ui(move |ui| show(ui, egui::Id::new("card"), &shown));
    harness.run();

    for label in ["Login variants", "Compare the layouts"] {
        assert!(harness.query_by_label_contains(label).is_some(), "{label}");
    }
    assert!(
        harness
            .query_by_label_contains("Artifact unavailable: artifact-gone")
            .is_some()
    );
    // Only stored artifacts can be opened.
    let open = harness.query_all_by_label("Open").collect::<Vec<_>>();
    assert_eq!(open.len(), 2);
    open[1].click();
    harness.run();
    assert_eq!(take_open_requests(&harness.ctx), vec![html]);
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
