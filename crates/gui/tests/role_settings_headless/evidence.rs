#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_role_routing_states() {
    // Given: both supported viewport sizes with a legacy binding and implicit routing.
    for size in [[960.0, 600.0], [1200.0, 900.0]] {
        let temp = tempfile::tempdir().expect("temp");
        let (_, _) = super::fixture(temp.path());
        let path = temp.path().join("evorch.toml");
        let text = std::fs::read_to_string(&path).expect("config");
        std::fs::write(
            &path,
            format!(
                "{}\n[agents.worker]\nlogical_model = 'legacy'",
                text.split("[routing.routes]").next().expect("profiles")
            ),
        )
        .expect("write");
        let (mut harness, _) = super::support::sized_fixture(temp.path(), size);
        harness.run();
        harness.click_label("Worker");
        harness.run();
        // When: capturing the warning, preview and prefilled routing transition.
        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/gui-evidence/role-settings");
        std::fs::create_dir_all(&output).expect("evidence directory");
        for state in ["warning-preview", "prefilled-routing"] {
            if state == "prefilled-routing" {
                harness.click_label("route を作成");
                harness.run();
            }
            let frame = gui::evidence::capture_or_skip(&mut harness).expect("offscreen capture");
            frame
                .save_png(&output.join(format!("{state}-{}.png", size[0])))
                .expect("PNG");
            // Then: capture dimensions match the actual viewport, with accessible primary action.
            assert_eq!(frame.width.to_string(), size[0].to_string());
            assert!(harness.has_label(if state == "prefilled-routing" {
                "Save routing"
            } else {
                "Save role settings"
            }));
        }
    }
}
