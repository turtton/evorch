use gui::headless::HeadlessWorkbench;
use runtime::{AgentModel, Role};

#[path = "routing_settings_headless/saves.rs"]
mod saves;
#[path = "routing_settings_headless/support.rs"]
mod support;
#[path = "routing_settings_headless/ux.rs"]
mod ux;
use support::{finish, fixture};

#[test]
fn route_row_lists_roles_using_that_name() {
    // Given: ロール使用先を注入した論理モデルの行。
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .add_route("shared")
        .expect("route");
    state.routing_settings_mut().route_users.insert(
        "shared".into(),
        vec!["explorer".into(), "worker.categories.quick".into()],
    );
    // When: ルーティング画面を描画する。
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // Then: この名前の使用先を表示する。
    assert!(harness.has_label("Used by: explorer, worker.categories.quick"));
}

#[test]
fn route_row_keeps_users_visible_during_rename() {
    // Given: a shared route whose name is being edited.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!(
            "{text}\n[routing.routes]\nshared = [{{profile = 'local'}}]\n\
             [agents.explorer]\nlogical_model = 'shared'\n\
             [agents.worker.categories.quick]\nlogical_model = 'shared'\n"
        ),
    )
    .expect("shared bindings");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .route_name_edits
        .insert("shared".into(), "renamed".into());
    // When: rendering the draft rename.
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // Then: users are still looked up by the original route name.
    assert!(harness.has_label("Used by: explorer, worker.categories.quick"));
    assert!(!harness.has_label("Used by: none"));
}

#[test]
fn draft_rename_warns_about_implicit_role_name_lookup_before_save() {
    // Given: worker uses its implicit role-name fallback and has a route.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!("{text}\n[routing.routes]\nworker = [{{profile = 'local'}}]\n"),
    )
    .expect("route");
    state.open_routing_settings();
    let warning = "Renaming leaves implicit role-name lookups unrouted: worker. Assign them explicitly in Role settings to keep them working.";
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    assert!(!harness.has_label(warning));
    // When: renaming the collapsed route in the draft, without saving.
    harness
        .state_mut()
        .routing_settings_mut()
        .rename_route("worker", "renamed")
        .expect("rename");
    harness.run();
    // Then: the warning is rendered even when the candidate editor is collapsed.
    assert!(harness.has_label(warning));
    assert!(harness.state().routing_settings().expanded.is_empty());
    // When: reverting the draft text.
    harness
        .state_mut()
        .routing_settings_mut()
        .route_name_edits
        .insert("worker".into(), "worker".into());
    harness.run();
    // Then: identity edits do not warn.
    assert!(!harness.has_label(warning));
}

#[test]
fn draft_rename_of_explicit_role_reference_does_not_warn_about_fallback() {
    // Given: worker explicitly references the route named worker.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!(
            "{text}\n[routing.routes]\nworker = [{{profile = 'local'}}]\n\
             [agents.worker]\nlogical_model = 'worker'\n"
        ),
    )
    .expect("route and explicit binding");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("worker", "renamed")
        .expect("rename");
    // When: rendering the rename that will rewrite the explicit binding.
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // Then: only the explicit user label is shown; no fallback-breakage warning.
    assert!(harness.has_label("Used by: worker"));
    assert!(!harness.has_label("Renaming leaves implicit role-name lookups unrouted: worker. Assign them explicitly in Role settings to keep them working."));
}

#[test]
fn empty_routes_without_providers_banner_in_routing_pane() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    std::fs::write(temp.path().join("evorch.toml"), "").expect("empty config");
    state.open_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    assert!(
        harness.has_label("No provider profiles and no routes: logical models cannot resolve.")
    );
}

#[test]
fn empty_routes_banner_in_routing_pane() {
    // Given: codex が先頭で既定モデルが一覧の先頭と異なる設定。
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    std::fs::write(
        temp.path().join("evorch.toml"),
        r#"
[providers.codex]
type = "openai-codex"
models = ["gpt-5.2", "gpt-5.3-codex"]
default_model = "gpt-5.3-codex"
"#,
    )
    .expect("config");
    state.open_routing_settings();
    // When: 明示ルートのない画面を描画する。
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // Then: 未定義の論理モデルは使用時に失敗することを説明する。
    assert!(harness.has_label(
        "No routes configured. Logical models without an explicit route fail when used. Add routes for each role or custom name below."
    ));
}

#[test]
fn prefilled_route_row_from_role_settings_is_editable() {
    // Given: ロール設定が開いている状態。
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_role_settings();
    state.open_routing_settings_prefill("new-role-model");
    assert!(!state.role_settings().open);
    assert!(!state.provider_settings().open);
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: プリフィルされた候補のモデルをドロップダウンで選択する。
    harness.scroll_label_into_view("new-role-model candidate 1 model override");
    harness.run();
    harness.click_label("new-role-model candidate 1 model override");
    harness.run();
    harness.click_label("fast");
    harness.run();
    // Then: 編集可能な候補と新規ルートマーカーが存在する。
    let model = harness.state().routing_settings();
    assert_eq!(
        model.routes["new-role-model"][0].model.as_deref(),
        Some("fast")
    );
    assert_eq!(model.pending_new_route.as_deref(), Some("new-role-model"));
}

#[test]
fn menu_add_reorder_save_rebuilds_runtime() {
    // Given: 実運用の再構成コンテキストと閉じた設定画面。
    let temp = tempfile::tempdir().expect("temp");
    let (state, model) = fixture(temp.path());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // When: メニューで開き候補を追加・移動して保存する。
    harness.click_label("⚙");
    harness.run();
    harness.click_label("Routing");
    harness.run();
    harness.state_mut().routing_settings_mut().new_route_name = "worker".into();
    harness.click_label("Add route");
    harness.run();
    harness.click_label("Add candidate");
    harness.run();
    harness.scroll_label_into_view("worker candidate 2 profile");
    harness.run();
    harness.click_label("worker candidate 2 profile");
    harness.run();
    harness.click_label("local");
    harness.run();
    harness.scroll_label_into_view("Move candidate 2 up");
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
            harness.click_label("worker");
            harness.run();
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
