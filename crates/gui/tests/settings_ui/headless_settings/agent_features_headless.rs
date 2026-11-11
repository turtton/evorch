use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};

const INITIAL: &str = "version = 3\n[metrics]\nenabled = false\n[sandbox]\nescalation_approval = \"user\"\n[tools.run_js]\nmax_tool_calls = 50\n";

struct Fixture {
    harness: HeadlessWorkbench<DemoSource>,
    runtime: runtime::AgentRuntime,
    path: std::path::PathBuf,
    options: config::LoadOptions,
}

/// The workbench over `dir`'s config with a live runtime whose run_js limits
/// differ from the file, as after the file was edited since startup.
fn fixture(dir: &std::path::Path) -> Fixture {
    let path = config::project_main_config_path(dir);
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
    std::fs::write(&path, INITIAL).expect("fixture");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        std::sync::Arc::new(tools::ToolExecutor::with_standard_tools(
            bus,
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        std::sync::Arc::new(runtime::compose::UnconfiguredModel),
    );
    runtime.set_web_tools_enabled(true);
    runtime.set_run_js_settings(config::RunJsConfig {
        max_tool_calls: 7,
        ..Default::default()
    });
    let options = fixture_options(dir);
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_settings_load_options(options.clone())
        .with_provider_settings_path(path.clone())
        .with_sandbox_runtime(runtime.clone());
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    Fixture {
        harness,
        runtime,
        path,
        options,
    }
}

fn open_from_settings_menu(harness: &mut HeadlessWorkbench<DemoSource>) {
    harness.click_label("⚙");
    harness.run();
    harness.click_label("Agent features");
    harness.run();
}

#[test]
fn agent_features_toggle_persists_to_the_user_config_and_applies_live() {
    // Given: run_js off and Web tools on, on disk and in the live runtime.
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        options,
        ..
    } = fixture(dir.path());
    // When: switching both through the settings menu's modal and saving.
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    harness.click_label("Web tools");
    harness.run();
    harness.click_label("Save agent features");
    harness.run();
    assert!(!harness.has_label("Save agent features"));
    // Then: only the two switches change on disk.
    let saved = config::Config::load_strict(&options).expect("reload");
    assert_eq!(
        saved.tools.run_js,
        config::RunJsConfig {
            enabled: true,
            max_tool_calls: 50,
            ..Default::default()
        }
    );
    assert!(!saved.sandbox.web_tools_enabled);
    assert_eq!(
        saved.sandbox.escalation_approval,
        config::EscalationApproval::User
    );
    assert!(!saved.metrics.enabled);
    // Then: the same runtime offers run_js to new runs with its limits kept,
    // and Web tools are off.
    assert_eq!(
        runtime.run_js_settings(),
        config::RunJsConfig {
            enabled: true,
            max_tool_calls: 7,
            ..Default::default()
        }
    );
    assert!(!runtime.web_tools_enabled());
}

#[test]
fn agent_features_cancel_discards_the_draft() {
    // Given: the modal with run_js switched on but not saved.
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        path,
        options,
    } = fixture(dir.path());
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    // When: cancelling, then reopening and saving without edits.
    harness.click_label("Cancel");
    harness.run();
    assert!(!runtime.run_js_enabled());
    assert_eq!(std::fs::read_to_string(&path).expect("read"), INITIAL);
    open_from_settings_menu(&mut harness);
    harness.click_label("Save agent features");
    harness.run();
    // Then: the persisted switches are kept on disk and in the runtime.
    let saved = config::Config::load_strict(&options).expect("reload");
    assert_eq!(
        config::AgentFeatures::from_config(&saved),
        config::AgentFeatures::default()
    );
    assert!(!runtime.run_js_enabled());
    assert!(runtime.web_tools_enabled());
}

#[test]
fn agent_features_save_refused_by_config_leaves_the_runtime_unchanged() {
    // Given: a user drop-in that pins run_js off over the main file.
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        path,
        ..
    } = fixture(dir.path());
    let dropin = dir
        .path()
        .join(config::PROJECT_CONFIG_DIR)
        .join("config.d/tools.toml");
    std::fs::create_dir_all(dropin.parent().expect("drop-in parent")).expect("drop-in dir");
    std::fs::write(&dropin, "[tools.run_js]\nenabled = false\n").expect("drop-in");
    // When: trying to switch run_js on.
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    harness.click_label("Save agent features");
    harness.run();
    // Then: the modal stays open, and neither the file nor the runtime changes.
    assert!(harness.has_label("Save agent features"));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), INITIAL);
    assert!(!runtime.run_js_enabled());
    assert!(runtime.web_tools_enabled());
}

#[test]
fn agent_features_scopes_persist_to_the_user_config_and_apply_live() {
    // Given: run_js off with the default scopes, on disk and in the live runtime.
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        path,
        options,
    } = fixture(dir.path());
    // When: switching run_js on, unchecking worker:deep and checking worker:quick
    // and the whole reviewer role, then saving.
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    harness.click_label("worker:deep");
    harness.run();
    harness.click_label("worker:quick");
    harness.run();
    harness.scroll_label_into_view("reviewer (all categories)");
    harness.run();
    harness.click_label("reviewer (all categories)");
    harness.run();
    harness.click_label("Save agent features");
    harness.run();
    assert!(!harness.has_label("Save agent features"));
    // Then: the file holds the edited scopes, with the neighbouring keys kept.
    let expected = ["worker:high-reasoning", "worker:quick", "reviewer"];
    let saved = config::Config::load_strict(&options).expect("reload");
    assert_eq!(saved.tools.run_js.scopes, expected);
    assert!(saved.tools.run_js.enabled);
    assert_eq!(saved.tools.run_js.max_tool_calls, 50);
    assert!(
        std::fs::read_to_string(&path)
            .expect("read")
            .contains("[metrics]")
    );
    // Then: the live runtime offers run_js to exactly those, with its limits kept.
    assert_eq!(runtime.run_js_settings().scopes, expected);
    assert_eq!(runtime.run_js_settings().max_tool_calls, 7);
    use agents::Role;
    assert!(runtime.run_js_offered_to(Role::Worker, Some("quick")));
    assert!(runtime.run_js_offered_to(Role::Reviewer, None));
    assert!(!runtime.run_js_offered_to(Role::Worker, Some("deep")));
    assert!(!runtime.run_js_offered_to(Role::Worker, None));
}

#[test]
fn agent_features_listing_a_whole_role_replaces_its_category_entries() {
    // Given: the default scopes (two worker categories).
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        options,
        ..
    } = fixture(dir.path());
    // When: switching run_js on and listing the whole worker role.
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    harness.click_label("worker (all categories)");
    harness.run();
    harness.click_label("Save agent features");
    harness.run();
    // Then: only the role is stored, and it covers every category and none.
    let saved = config::Config::load_strict(&options).expect("reload");
    assert_eq!(saved.tools.run_js.scopes, ["worker"]);
    assert!(runtime.run_js_offered_to(agents::Role::Worker, None));
    assert!(runtime.run_js_offered_to(agents::Role::Worker, Some("quick")));
}

#[test]
fn agent_features_cancel_discards_scope_edits() {
    // Given: the modal with run_js on and a scope edited but not saved.
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        path,
        ..
    } = fixture(dir.path());
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    harness.click_label("worker (all categories)");
    harness.run();
    // When: cancelling.
    harness.click_label("Cancel");
    harness.run();
    // Then: neither the file nor the runtime changed.
    assert_eq!(std::fs::read_to_string(&path).expect("read"), INITIAL);
    assert_eq!(
        runtime.run_js_settings().scopes,
        config::RunJsConfig::default().scopes
    );
    assert!(!runtime.run_js_enabled());
}

#[test]
fn agent_features_scope_save_refused_by_config_leaves_the_runtime_unchanged() {
    // Given: a user drop-in that pins different scopes over the main file.
    let dir = tempfile::tempdir().expect("temp");
    let Fixture {
        mut harness,
        runtime,
        path,
        ..
    } = fixture(dir.path());
    let dropin = dir
        .path()
        .join(config::PROJECT_CONFIG_DIR)
        .join("config.d/tools.toml");
    std::fs::create_dir_all(dropin.parent().expect("drop-in parent")).expect("drop-in dir");
    std::fs::write(&dropin, "[tools.run_js]\nscopes = [\"advisor\"]\n").expect("drop-in");
    // When: editing scopes and saving.
    open_from_settings_menu(&mut harness);
    harness.click_label("run_js (prototype)");
    harness.run();
    harness.click_label("worker (all categories)");
    harness.run();
    harness.click_label("Save agent features");
    harness.run();
    // Then: the modal stays open, and neither the file nor the runtime changes.
    assert!(harness.has_label("Save agent features"));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), INITIAL);
    assert_eq!(
        runtime.run_js_settings().scopes,
        config::RunJsConfig::default().scopes
    );
}

fn fixture_options(dir: &std::path::Path) -> config::LoadOptions {
    config::LoadOptions {
        project_dir: Some(dir.to_path_buf()),
        user_config_dir: Some(dir.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    }
}

#[test]
#[ignore = "captures native GPU evidence"]
fn capture_agent_features_settings() {
    for size in [[960.0, 600.0], [640.0, 480.0]] {
        // Given: the Agent features modal at the given geometry, run_js on.
        let dir = tempfile::tempdir().expect("temp");
        let path = config::project_main_config_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
        std::fs::write(&path, "version = 3\n[tools.run_js]\nenabled = true\n").expect("config");
        let mut state =
            WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                .expect("state")
                .with_settings_load_options(fixture_options(dir.path()))
                .with_provider_settings_path(path);
        state.open_agent_features();
        let mut harness = HeadlessWorkbench::new(state, size);
        harness.run();
        let save = |harness: &mut HeadlessWorkbench<DemoSource>, name: &str| {
            let frame = harness.capture().expect("capture");
            frame
                .save_png(&std::env::temp_dir().join(format!(
                    "agent-features-settings-{name}-{}x{}.png",
                    size[0], size[1]
                )))
                .expect("PNG");
        };
        // When: capturing the real egui rendering, at the top and scrolled to the scopes.
        save(&mut harness, "top");
        harness.scroll_label_into_view("reviewer (all categories)");
        harness.run();
        save(&mut harness, "scopes");
        // Then: every switch, the scope rows and the actions fit within the viewport
        // once scrolled to.
        let fits = |harness: &HeadlessWorkbench<DemoSource>, label: &str| {
            let rects = harness.label_rects(label);
            assert!(!rects.is_empty(), "missing {label}");
            assert!(
                rects
                    .iter()
                    .all(|rect| harness.screen_rect().contains_rect(*rect)),
                "{label} is outside the viewport"
            );
        };
        for label in [
            "Web tools",
            "run_js (prototype)",
            "worker (all categories)",
            "worker:deep",
            "worker:high-reasoning",
            "reviewer (all categories)",
        ] {
            harness.scroll_label_into_view(label);
            harness.run();
            fits(&harness, label);
        }
        for label in ["Save agent features", "Cancel"] {
            fits(&harness, label);
        }
    }
}
