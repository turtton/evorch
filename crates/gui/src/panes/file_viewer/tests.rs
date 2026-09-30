use super::*;

#[test]
fn resolves_editor_and_file_uri_links_without_consuming_external_urls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello world.rs");
    std::fs::write(&path, "fn main() {}\n").unwrap();
    for link in ["hello%20world.rs:12:3", "./hello%20world.rs#L12-L20"] {
        assert_eq!(
            resolve_file_link(link, Some(dir.path())),
            Some(FileLink {
                path: path.clone(),
                line: Some(12)
            })
        );
    }
    let uri = url::Url::from_file_path(&path).unwrap();
    assert_eq!(
        resolve_file_link(&format!("{uri}#L8"), None),
        Some(FileLink {
            path: path.clone(),
            line: Some(8)
        })
    );
    assert_eq!(
        resolve_file_link(&format!("{}:5", path.display()), None)
            .unwrap()
            .line,
        Some(5)
    );
    for url in [
        "https://example.com:8080/a.rs#L4",
        "mailto:me@example.com",
        "#heading",
        "//example.com/foo",
        "data:text/plain,x",
    ] {
        assert!(resolve_file_link(url, Some(dir.path())).is_none(), "{url}");
    }
    assert!(resolve_file_link("relative.rs", None).is_none());
}

#[test]
fn only_this_panes_local_open_commands_are_consumed() {
    let ctx = egui::Context::default();
    ctx.open_url(egui::OpenUrl::new_tab("outside.rs"));
    ctx.open_url(egui::OpenUrl::new_tab("source.rs:4"));
    ctx.open_url(egui::OpenUrl::new_tab("https://example.com"));
    ctx.copy_text("copy".into());
    let links = take_file_links(&ctx, 1, Some(Path::new("/tmp")));
    assert_eq!(
        links,
        [FileLink {
            path: "/tmp/source.rs".into(),
            line: Some(4)
        }]
    );
    ctx.output(|output| {
        assert_eq!(output.commands.len(), 3);
        assert!(matches!(&output.commands[0], egui::OutputCommand::OpenUrl(url) if url.url == "outside.rs"));
        assert!(matches!(&output.commands[1], egui::OutputCommand::OpenUrl(url) if url.url == "https://example.com"));
        assert!(matches!(&output.commands[2], egui::OutputCommand::CopyText(text) if text == "copy"));
    });
}

#[test]
fn loads_text_png_and_jpeg_and_rejects_oversized_or_binary_files() {
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("main.rs");
    std::fs::write(&text, "fn main() {}\n").unwrap();
    assert!(matches!(load_file(&text), Ok(LoadedFile::Text(source)) if source.contains("fn main")));
    for extension in ["png", "jpg"] {
        let path = dir.path().join(format!("image.{extension}"));
        image::RgbImage::new(4, 3).save(&path).unwrap();
        assert!(matches!(load_file(&path), Ok(LoadedFile::Image(image)) if image.size == [4, 3]));
    }
    let large_image = dir.path().join("too-wide.png");
    image::RgbImage::new(MAX_IMAGE_SIDE + 1, 1)
        .save(&large_image)
        .unwrap();
    assert!(load_file(&large_image).is_err());
    let too_big = dir.path().join("too-big");
    std::fs::File::create(&too_big)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(load_file(&too_big).is_err());
    let binary = dir.path().join("binary");
    std::fs::write(&binary, b"a\0b").unwrap();
    assert!(load_file(&binary).is_err());
    assert!(load_file(dir.path()).is_err());
    assert!(load_file(&dir.path().join("missing")).is_err());
}

#[test]
fn source_preview_highlights_keywords_and_markdown_renders_local_images() {
    use egui_kittest::{Harness, kittest::Queryable};
    let dir = tempfile::tempdir().unwrap();
    let markdown = dir.path().join("readme.md");
    std::fs::write(
        &markdown,
        "# Preview heading\n\n![Local preview](picture.png)\n\n| Image |\n| --- |\n| ![Table preview](picture.png) |\n",
    )
    .unwrap();
    image::RgbImage::new(4, 3)
        .save(dir.path().join("picture.png"))
        .unwrap();
    let mut harness = Harness::new_ui(move |ui| file_viewer_pane(ui, &markdown));
    for _ in 0..100 {
        harness.run_steps(1);
        if image_shapes(&harness) >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(harness.query_by_label("Preview heading").is_some());
    assert!(
        image_shapes(&harness) >= 2,
        "standalone and table images must render"
    );

    let ctx = egui::Context::default();
    let style = egui::Style::default();
    let job = highlight(
        &ctx,
        &style,
        &CodeTheme::from_style(&style),
        "fn main() { let text = \"hi\"; }",
        "rs",
    );
    let colors: std::collections::HashSet<_> = job
        .sections
        .iter()
        .map(|section| section.format.color)
        .collect();
    assert!(colors.len() > 1, "source must have syntax colors");
}

fn image_shapes(harness: &egui_kittest::Harness<'_, ()>) -> usize {
    harness.output().shapes.iter().filter(|shape| {
        matches!(&shape.shape, egui::epaint::Shape::Rect(rect) if rect.brush.as_ref().is_some_and(|brush| matches!(brush.fill_texture_id, egui::TextureId::Managed(id) if id > 0)))
    }).count()
}
