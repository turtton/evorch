#[test]
fn ac14_overlays_guide_materials_html_discretion_and_delegation() {
    let visual = include_str!("../assets/presets/category-visual.md");
    let conversation = include_str!("../assets/presets/category-conversation.md");
    for overlay in [visual, conversation] {
        for term in [
            "generate_image",
            "variant",
            "相対パス",
            "render_artifact",
            "HTML",
            "UI モックの代わりにしない",
            "参考",
            "$TMPDIR",
            "reference_image_paths",
            "OpenAI",
        ] {
            assert!(overlay.contains(term), "missing {term}");
        }
    }
    assert!(conversation.contains("Orchestrator"));
    assert!(conversation.contains("visual Worker"));
    assert!(conversation.contains("委譲"));
}

#[test]
fn ac14_documented_image_chain_config_loads() {
    let document = include_str!("../../../docs/image-generation.md");
    let toml = document
        .split("```toml\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!("version = 3\n{toml}"),
    )
    .unwrap();
    let config = config::Config::load_strict(&config::LoadOptions {
        user_config_dir: Some(dir.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(config.agents.image_generator_chain().len(), 2);
    for route in ["codex-image", "openai-image"] {
        assert_eq!(
            config.routing.routes[route][0].kind(&config.providers),
            Some(config::ModelKind::ImageGeneration)
        );
    }
}
