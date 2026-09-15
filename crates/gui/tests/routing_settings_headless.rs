use std::sync::Arc;

use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::compose::{SwitchableModel, UnconfiguredModel};
use runtime::{AgentModel, Role};

fn fixture(root: &std::path::Path) -> (WorkbenchState<DemoSource>, Arc<SwitchableModel>) {
    std::fs::write(
        root.join("evorch.toml"),
        r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base", "fast"]
default_model = "base"
[providers.accelerated]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["fast"]
default_model = "fast"
"#,
    )
    .expect("fixture");
    let context = gui::model::production::ProductionModel {
        load_options: config::LoadOptions {
            project_dir: Some(root.into()),
            user_config_dir: Some(root.join("user")),
            read_env: false,
            ..Default::default()
        },
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(root.join("credentials"))
                .expect("store"),
        ),
        bus: Arc::new(event_bus::EventBus::new(32)),
        env: Arc::new(routing::MapEnv::new(
            [("TEST_KEY".into(), "secret".into())].into(),
        )),
    };
    let model = Arc::new(SwitchableModel::new(Arc::new(UnconfiguredModel)));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_provider_settings_path(root.join("evorch.toml"))
        .with_production_model(context, model.clone());
    (state, model)
}

fn finish(harness: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.state().routing_settings().is_saving() {
        assert!(std::time::Instant::now() < deadline, "save timeout");
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
}

#[test]
fn menu_add_reorder_save_rebuilds_runtime() {
    // Given: 実運用の再構成コンテキストと閉じた設定画面。
    let temp = tempfile::tempdir().expect("temp");
    let (state, model) = fixture(temp.path());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // When: メニューで開き候補を追加・移動して保存する。
    harness.click_label("Workbench settings");
    harness.run();
    harness.click_label("Routing");
    harness.run();
    harness.state_mut().routing_settings_mut().new_route_name = "worker".into();
    harness.click_label("Add route");
    harness.run();
    harness.click_label("Add candidate");
    harness.run();
    harness.click_label("worker candidate 2 profile");
    harness.run();
    harness.click_label("local");
    harness.run();
    harness.click_label("Move candidate 2 up");
    harness.run();
    let expected = harness
        .state()
        .routing_settings()
        .validated_routing()
        .expect("valid");
    harness.click_label("Save routing");
    harness.step();
    finish(&mut harness);
    // Then: ディスク・再 seed・実 Router の優先候補が一致する。
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("saved");
    assert_eq!(saved.routing, expected);
    assert_eq!(harness.state().routing_settings().routes, expected.routes);
    assert_eq!(saved.routing.routes["worker"][0].profile, "local");
    assert_eq!(saved.routing.routes["worker"][1].profile, "accelerated");
    assert_eq!(model.selected_model(Role::Worker, None), "local/base");
    assert!(harness.state().routing_settings().open);
}

#[test]
fn invalid_save_keeps_disk_and_cancel_reloads() {
    // Given: 未知プロファイルを含む編集内容。
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_routing_settings();
    state.routing_settings_mut().routes.insert(
        "worker".into(),
        vec![config::RouteCandidateConfig::default()],
    );
    let before = std::fs::read(temp.path().join("evorch.toml")).expect("read");
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: 保存を試みる。
    harness.click_label("Save routing");
    harness.run();
    // Then: エラーが可視でディスクは不変。
    let error = harness
        .state()
        .routing_settings()
        .validation_error
        .as_deref()
        .expect("error");
    assert!(harness.has_label(error));
    assert_eq!(
        std::fs::read(temp.path().join("evorch.toml")).expect("read"),
        before
    );
    harness.click_label("Cancel");
    harness.run();
    assert!(!harness.state().routing_settings().open);
    harness.state_mut().open_routing_settings();
    assert!(harness.state().routing_settings().routes.is_empty());
}

#[test]
fn settings_are_mutually_exclusive() {
    // Given: routing 設定を開く。
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_routing_settings();
    // When: 別の設定を順に開く。
    state.open_provider_settings();
    assert!(!state.routing_settings().open);
    state.open_routing_settings();
    state.open_role_settings();
    assert!(!state.routing_settings().open);
    state.open_routing_settings();
    // Then: 最後に開いた設定だけが表示される。
    assert!(!state.role_settings().open);
    assert!(!state.provider_settings().open);
    assert!(state.routing_settings().open);
}

#[test]
fn many_candidates_fit_geometry_matrix() {
    // Given: 多数の候補を持つ設定と各画面サイズ・DPI。
    for size in [[960.0, 600.0], [1200.0, 900.0]] {
        for dpi in [1.0, 1.5] {
            let temp = tempfile::tempdir().expect("temp");
            let (mut state, _) = fixture(temp.path());
            state.open_routing_settings();
            state.routing_settings_mut().routes.insert(
                "worker".into(),
                vec![
                    config::RouteCandidateConfig {
                        profile: "local".into(),
                        model: None
                    };
                    20
                ],
            );
            let mut harness = HeadlessWorkbench::with_pixels_per_point(state, size, dpi);
            harness.run();
            // When: 最後の候補までスクロールする。
            harness.scroll_label_into_view("Move candidate 20 up");
            harness.run();
            // Then: 最終行と固定フッターが画面内に収まる。
            for label in ["Move candidate 20 up", "Save routing", "Cancel"] {
                let rects = harness.label_rects(label);
                assert!(!rects.is_empty(), "{label}");
                assert!(
                    rects
                        .iter()
                        .all(|rect| harness.screen_rect().contains_rect(*rect)),
                    "{size:?}/{dpi}: {label}: {rects:?}"
                );
            }
        }
    }
}

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_routing_settings_png_evidence() {
    // Given: 複数候補を持つ実モーダル。
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .add_route("worker")
        .expect("route");
    state
        .routing_settings_mut()
        .routes
        .get_mut("worker")
        .expect("worker")
        .push(config::RouteCandidateConfig {
            profile: "local".into(),
            model: Some("base".into()),
        });
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: 実 egui 描画を取得する。
    let frame = harness.capture().expect("GPU capture");
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gui-evidence/routing-settings");
    std::fs::create_dir_all(&output).expect("directory");
    frame
        .save_png(&output.join("routing-settings.png"))
        .expect("PNG");
    // Then: 指定寸法で保存操作が可視。
    assert_eq!((frame.width, frame.height), (960, 600));
    assert!(harness.has_label("Save routing"));
}
