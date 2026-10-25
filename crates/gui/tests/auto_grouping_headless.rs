//! Saving a provider adds its models to the pools of the same models.dev model
//! (ADR 0030), asking the user when one profile offers several.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::provider_settings::ProviderSettingsModel;
use gui::model::transcript::TranscriptEntry;
use workspace_ui::UiSettings;

const POOL: &str = "moonshotai/kimi-k3";

const PROVIDERS: &str = r#"
[providers.vivgrid]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["kimi-k3"]
default_model = "kimi-k3"
[providers.neuralwatt]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["kimi-k3"]
default_model = "kimi-k3"
[providers.multi]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["kimi-k3", "kimi-k3-256k"]
default_model = "kimi-k3"
"#;

const ROUTES: &str = r#"
[routing.routes]
"moonshotai/kimi-k3" = [{ profile = "vivgrid" }]
[agents.roles.planner]
models = [{ route = "moonshotai/kimi-k3" }]
"#;

async fn catalog(neuralwatt_canonical: &str) -> catalog::ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let model =
        |id: &str, canonical: &str| serde_json::json!({"id": id, "canonical_model_id": canonical});
    let api = serde_json::json!({
        "vivgrid": {"models": {"kimi-k3": model("kimi-k3", POOL)}},
        "neuralwatt": {"models": {"kimi-k3": model("kimi-k3", neuralwatt_canonical)}},
        "multi": {"models": {
            "kimi-k3": model("kimi-k3", POOL),
            "kimi-k3-256k": model("kimi-k3-256k", POOL),
        }},
    });
    std::fs::write(
        dir.path().join("models-dev.json"),
        serde_json::json!({"fetched_at": 4_102_444_800_u64, "api": api}).to_string(),
    )
    .unwrap();
    catalog::ModelCatalog::load_or_refresh(dir.path())
        .await
        .unwrap()
}

fn options(root: &Path) -> config::LoadOptions {
    config::LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    }
}

fn write(root: &Path, content: &str) {
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(config::project_main_config_path(root), content).unwrap();
}

fn workbench(root: &Path, catalog: catalog::ModelCatalog) -> HeadlessWorkbench<DemoSource> {
    let config = config::Config::load(&options(root)).unwrap();
    let mut settings = ProviderSettingsModel::seed_from_config(&config);
    settings.catalog.catalog = Some(Arc::new(catalog));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_settings_load_options(options(root))
        .with_provider_settings_path(config::project_main_config_path(root))
        .with_provider_settings(settings);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn save(harness: &mut HeadlessWorkbench<DemoSource>, profile: &str) {
    harness.state_mut().open_provider_settings();
    harness.state_mut().provider_settings_mut().edit(profile);
    harness.run();
    harness.click_label("Save");
    harness.wait_provider_save(Duration::from_secs(10));
    harness.run();
}

fn pool(root: &Path) -> Vec<config::RouteCandidateConfig> {
    config::Config::load(&options(root)).unwrap().routing.routes[POOL].clone()
}

fn candidate(profile: &str, model: Option<&str>, auto: bool) -> config::RouteCandidateConfig {
    config::RouteCandidateConfig {
        profile: profile.into(),
        model: model.map(Into::into),
        auto,
    }
}

fn notices(harness: &HeadlessWorkbench<DemoSource>) -> Vec<String> {
    harness
        .state()
        .transcripts()
        .thread()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Notice { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// Given: a pool the planner uses, served by vivgrid
// When: neuralwatt, which serves the same canonical model, is saved
// Then: it joins the pool at the lowest priority as an auto candidate, and says so
#[tokio::test]
async fn saving_a_provider_adds_it_to_the_pool_of_the_same_model() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), &format!("{PROVIDERS}{ROUTES}"));
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    save(&mut harness, "neuralwatt");
    assert_eq!(harness.state().provider_settings().error, None);
    assert_eq!(
        pool(temp.path()),
        [
            candidate("vivgrid", None, false),
            candidate("neuralwatt", Some("kimi-k3"), true),
        ]
    );
    assert!(notices(&harness).iter().any(|text| text
        == "Added neuralwatt/kimi-k3 to route moonshotai/kimi-k3 as an automatic fallback path"));
}

#[tokio::test]
async fn auto_grouping_gui_save_keeps_split_image_registration_context() {
    let temp = tempfile::tempdir().unwrap();
    let document = format!("{PROVIDERS}{ROUTES}").replace(
        "[routing.routes]",
        "[routing.routes]\nart=[{profile='codex',model='studio'}]",
    );
    write(
        temp.path(),
        &format!("{document}\n[agents.roles.image_generator]\nmodels=[{{route='art'}}]\n"),
    );
    let user = temp.path().join(config::PROJECT_CONFIG_DIR);
    std::fs::create_dir(user.join("config.d")).unwrap();
    let dropin = user.join("config.d/providers.toml");
    let providers = "[providers.codex]\ntype='openai-codex'\nmodels=['chat',{id='studio',enabled=true,kind='image_generation'}]\ndefault_model='chat'\n";
    std::fs::write(&dropin, providers).unwrap();
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    save(&mut harness, "neuralwatt");
    assert_eq!(harness.state().provider_settings().error, None);
    assert_eq!(pool(temp.path()).len(), 2);
    let loaded = config::Config::load_strict(&options(temp.path())).unwrap();
    assert_eq!(loaded.agents.roles.image_generator.models[0].route, "art");
    assert_eq!(std::fs::read_to_string(&dropin).unwrap(), providers);
}

// Given: multi offers two models of the pool's canonical model
// When: it is saved and the user picks one
// Then: nothing is added until the choice, and only the chosen model joins
#[tokio::test]
async fn several_models_of_one_profile_are_offered_for_a_choice() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), &format!("{PROVIDERS}{ROUTES}"));
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    save(&mut harness, "multi");
    assert_eq!(pool(temp.path()), [candidate("vivgrid", None, false)]);
    assert!(harness.has_label("Choose a fallback path"));
    harness.click_label("Add kimi-k3-256k");
    harness.wait_provider_save(Duration::from_secs(10));
    harness.run();
    assert!(!harness.has_label("Choose a fallback path"));
    assert_eq!(
        pool(temp.path()),
        [
            candidate("vivgrid", None, false),
            candidate("multi", Some("kimi-k3-256k"), true),
        ]
    );
}

// Given: the user skipped a conflict
// When: the same provider is saved again
// Then: the prompt does not come back and nothing is written
#[tokio::test]
async fn a_skipped_choice_is_not_asked_again() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), &format!("{PROVIDERS}{ROUTES}"));
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    save(&mut harness, "multi");
    harness.click_label("Skip");
    harness.run();
    assert!(!harness.has_label("Choose a fallback path"));
    save(&mut harness, "multi");
    assert!(!harness.has_label("Choose a fallback path"));
    assert_eq!(pool(temp.path()), [candidate("vivgrid", None, false)]);
}

// Given: the pool is defined in a config.d drop-in, not in the file the GUI saves
// When: neuralwatt is saved
// Then: the drop-in's route is not copied into the saved file, and the user is told
#[tokio::test]
async fn a_pool_from_another_layer_is_not_copied_into_the_saved_file() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), PROVIDERS);
    let dropin = temp
        .path()
        .join(config::PROJECT_CONFIG_DIR)
        .join("config.d");
    std::fs::create_dir_all(&dropin).unwrap();
    std::fs::write(dropin.join("10-routes.toml"), ROUTES).unwrap();
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    save(&mut harness, "neuralwatt");
    assert_eq!(pool(temp.path()), [candidate("vivgrid", None, false)]);
    let saved = std::fs::read_to_string(config::project_main_config_path(temp.path())).unwrap();
    assert!(!saved.contains(POOL), "{saved}");
    assert!(notices(&harness).iter().any(|text| text
        == "Route moonshotai/kimi-k3 is defined in another config layer; neuralwatt/kimi-k3 was not added"));
}

// Given: an auto candidate whose models.dev canonical id has since changed
// When: the catalog is checked twice
// Then: one warning is shown and the configuration is left as it is
#[tokio::test]
async fn a_changed_canonical_id_warns_once_without_rewriting() {
    let temp = tempfile::tempdir().unwrap();
    let routes = ROUTES.replace(
        r#"[{ profile = "vivgrid" }]"#,
        r#"[{ profile = "vivgrid" }, { profile = "neuralwatt", model = "kimi-k3", auto = true }]"#,
    );
    write(temp.path(), &format!("{PROVIDERS}{routes}"));
    let before = std::fs::read_to_string(config::project_main_config_path(temp.path())).unwrap();
    let mut harness = workbench(temp.path(), catalog("moonshotai/kimi-k3.1").await);
    harness.state_mut().warn_stale_auto_candidates();
    harness.state_mut().warn_stale_auto_candidates();
    let warnings: Vec<_> = notices(&harness)
        .into_iter()
        .filter(|text| text.contains("neuralwatt/kimi-k3 as moonshotai/kimi-k3.1"))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(
        std::fs::read_to_string(config::project_main_config_path(temp.path())).unwrap(),
        before
    );
}

// Given: a pool with an automatically added candidate
// When: routing settings are opened
// Then: the candidate is marked as automatic
#[tokio::test]
async fn routing_settings_mark_automatic_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let routes = ROUTES.replace(
        r#"[{ profile = "vivgrid" }]"#,
        r#"[{ profile = "vivgrid" }, { profile = "neuralwatt", model = "kimi-k3", auto = true }]"#,
    );
    write(temp.path(), &format!("{PROVIDERS}{routes}"));
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    harness.state_mut().open_routing_settings();
    harness.run();
    harness.click_label(POOL);
    harness.run();
    assert_eq!(harness.count_labels("auto"), 1);
}

fn evidence_dir() -> std::path::PathBuf {
    let output =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui-evidence/auto-grouping");
    std::fs::create_dir_all(&output).expect("evidence directory");
    output
}

#[tokio::test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
async fn capture_auto_grouping_png_evidence() {
    // Given: a provider save that left a choice to the user.
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), &format!("{PROVIDERS}{ROUTES}"));
    let mut harness = workbench(temp.path(), catalog(POOL).await);
    save(&mut harness, "multi");
    // When: capturing the prompt, then routing settings after the choice.
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(&evidence_dir().join("conflict-prompt.png"))
        .expect("PNG");
    harness.click_label("Add kimi-k3-256k");
    harness.wait_provider_save(Duration::from_secs(10));
    harness.state_mut().open_routing_settings();
    harness.run();
    harness.click_label(POOL);
    harness.run();
    let frame = gui::evidence::capture_or_skip(&mut harness).expect("adapter stays available");
    frame
        .save_png(&evidence_dir().join("routing-auto-marker.png"))
        .expect("PNG");
    // Then: the chosen path is marked automatic.
    assert_eq!(harness.count_labels("auto"), 1);
}
