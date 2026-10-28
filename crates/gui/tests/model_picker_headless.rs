use std::sync::Arc;

use egui::Key;
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ProviderStatus;
use gui::model::production::{ProductionModel, compose_production_model};
use gui::panes::model_picker::{
    AUTOMATIC_LABEL, DEFAULT_EFFORT_LABEL, EFFORT_HEADING, SEARCH_LABEL,
};
use runtime::compose::SwitchableModel;
use workspace_ui::{ModelPreference, ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, configured: bool) -> HeadlessWorkbench<DemoSource> {
    workbench_with_provider(root, configured, "")
}

fn workbench_with_provider(
    root: &std::path::Path,
    configured: bool,
    extra_provider: &str,
) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar.add_project(project.clone(), "demo", root).unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["thread-1", "thread-2"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("thread-1")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"));
    if configured {
        std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
        std::fs::write(
            config::project_main_config_path(root),
            r#"
[providers.local]
type = "openai-compatible"
base_url = "http://localhost:11434/v1"
models = [
  { id = "model-a", enabled = true, effort_levels = ["minimal"] },
  { id = "model-b", enabled = true, effort_levels = ["low", "high"] },
]
default_model = "model-a"
[providers.remote]
type = "openai-compatible"
base_url = "https://example.test/v1"
models = ["model-c"]
default_model = "model-c"
"#
            .to_owned()
                + extra_provider,
        )
        .unwrap();
        let context = ProductionModel {
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            load_options: config::LoadOptions {
                project_dir: Some(root.to_owned()),
                user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
                read_env: false,
                ..Default::default()
            },
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(root.join("credentials")).unwrap(),
            ),
            bus: Arc::new(event_bus::EventBus::new(32)),
            env: Arc::new(
                [("ANTHROPIC_API_KEY", "test-secret")]
                    .into_iter()
                    .collect::<routing::MapEnv>(),
            ),
        };
        let config = config::Config::load(&context.load_options).unwrap();
        let model = compose_production_model(&config, &context).unwrap();
        state = state
            .with_production_model(context, Arc::new(SwitchableModel::new(model)))
            .with_provider_status(ProviderStatus::Configured);
    }
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn default_model(harness: &HeadlessWorkbench<DemoSource>) -> String {
    let model = harness
        .state()
        .composer()
        .resolved_model
        .clone()
        .expect("automatic routing resolves a default model");
    assert!(!harness.has_label("Select model"));
    model
}

fn open_picker(harness: &mut HeadlessWorkbench<DemoSource>, button: &str) {
    harness.click_label(button);
    harness.run();
    assert!(harness.has_label(SEARCH_LABEL), "the modal opens");
}

fn type_query(harness: &mut HeadlessWorkbench<DemoSource>, text: &str) {
    harness
        .input_mut()
        .events
        .push(egui::Event::Text(text.into()));
    harness.run();
}

fn press(harness: &mut HeadlessWorkbench<DemoSource>, key: Key) {
    harness.key_press(egui::Modifiers::NONE, key);
    harness.run();
}

fn preference(
    profile: &str,
    model: &str,
    reasoning_effort: Option<&str>,
) -> Option<ModelPreference> {
    Some(ModelPreference {
        profile: profile.into(),
        model: Some(model.into()),
        reasoning_effort: reasoning_effort.map(str::to_owned),
    })
}

fn thread_preference(harness: &HeadlessWorkbench<DemoSource>) -> Option<ModelPreference> {
    harness.state().sidebar().threads[0]
        .model_preference
        .clone()
}

#[test]
fn model_button_opens_a_modal_listing_profiles_and_models() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    assert!(!harness.has_label(SEARCH_LABEL));
    // When: the button naming the routed default model is pressed.
    let button = default_model(&harness);
    open_picker(&mut harness, &button);
    // Then
    for label in [
        AUTOMATIC_LABEL,
        "local / model-a",
        "local / model-b",
        "remote / model-c",
    ] {
        assert!(harness.has_label(label), "{label}");
    }
}

#[test]
fn search_then_enter_selects_the_model_and_persists_it() {
    // Given: the search box has focus as soon as the modal opens.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    let button = default_model(&harness);
    open_picker(&mut harness, &button);
    // When: searching by model name and confirming with Enter.
    type_query(&mut harness, "model-c");
    assert!(!harness.has_label("local / model-a"));
    press(&mut harness, Key::Enter);
    // Then: its effort step follows, and Enter on Default effort finishes the selection.
    assert!(harness.has_label(EFFORT_HEADING));
    assert_eq!(thread_preference(&harness), None);
    press(&mut harness, Key::Enter);
    let expected = preference("remote", "model-c", None);
    assert_eq!(thread_preference(&harness), expected);
    assert_eq!(harness.state().sidebar().threads[1].model_preference, None);
    assert!(!harness.has_label(SEARCH_LABEL));
    assert!(harness.state().issued().is_empty());
    let restored = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert_eq!(restored.threads[0].model_preference, expected);
}

#[test]
fn search_matches_provider_name_and_type_with_fuzzy_ranking() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    let button = default_model(&harness);
    open_picker(&mut harness, &button);
    // When: searching a profile name, then a provider type shared by both profiles.
    type_query(&mut harness, "remote");
    // Then
    assert!(harness.has_label("remote / model-c"));
    assert!(!harness.has_label("local / model-a"));
    assert!(!harness.has_label(AUTOMATIC_LABEL));
    // When: a subsequence of the model name still finds it, and nonsense finds nothing.
    for _ in 0.."remote".len() {
        press(&mut harness, Key::Backspace);
    }
    type_query(&mut harness, "mdlb");
    assert!(harness.has_label("local / model-b"));
    assert!(!harness.has_label("remote / model-c"));
    for _ in 0.."mdlb".len() {
        press(&mut harness, Key::Backspace);
    }
    type_query(&mut harness, "zzzz");
    assert!(harness.has_label("No matching models"));
}

#[test]
fn arrow_keys_move_the_highlight_and_escape_closes_without_changes() {
    // Given: automatic routing is current, so it starts highlighted.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    let button = default_model(&harness);
    open_picker(&mut harness, &button);
    // When: moving down to remote / model-c and confirming.
    for _ in 0..3 {
        press(&mut harness, Key::ArrowDown);
    }
    press(&mut harness, Key::Enter);
    // Then: Enter on the effort step's Default effort completes the selection.
    assert!(harness.has_label(EFFORT_HEADING));
    press(&mut harness, Key::Enter);
    assert_eq!(
        thread_preference(&harness),
        preference("remote", "model-c", None)
    );
    // When: the highlight moves after reopening, Escape closes without applying it.
    harness.click_label("remote / model-c");
    harness.run();
    press(&mut harness, Key::ArrowUp);
    press(&mut harness, Key::Escape);
    // Then
    assert!(!harness.has_label(SEARCH_LABEL));
    assert_eq!(
        thread_preference(&harness),
        preference("remote", "model-c", None)
    );
}

#[test]
fn effort_step_follows_model_levels_and_persists() {
    // Given: a model that restricts effort levels.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    let button = default_model(&harness);
    open_picker(&mut harness, &button);
    // When: choosing it from the list.
    harness.click_label("local / model-b");
    harness.run();
    // Then: the effort step offers only its levels, and nothing is stored yet.
    assert!(harness.has_label(EFFORT_HEADING));
    assert!(harness.has_label(DEFAULT_EFFORT_LABEL));
    assert!(harness.has_label("low"));
    assert!(!harness.has_label("xhigh"));
    assert_eq!(thread_preference(&harness), None);
    // When: choosing an effort.
    harness.click_label("high");
    harness.run();
    // Then: model and effort are stored together.
    let expected = preference("local", "model-b", Some("high"));
    assert_eq!(thread_preference(&harness), expected);
    assert!(!harness.has_label(EFFORT_HEADING));
    let restored = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert_eq!(restored.threads[0].model_preference, expected);
    // When: switching to a model that cannot send that effort.
    open_picker(&mut harness, "local / model-b · high");
    harness.click_label("local / model-a");
    harness.run();
    // Then: the effort step starts on Default effort, so Enter drops the effort.
    assert!(harness.has_label(EFFORT_HEADING));
    press(&mut harness, Key::Enter);
    assert_eq!(
        thread_preference(&harness),
        preference("local", "model-a", None)
    );
}

#[test]
fn escape_in_the_effort_step_returns_to_the_model_list() {
    // Given: the effort step of a model with levels.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    let button = default_model(&harness);
    open_picker(&mut harness, &button);
    harness.click_label("local / model-b");
    harness.run();
    assert!(harness.has_label(EFFORT_HEADING));
    // When: Escape is pressed.
    press(&mut harness, Key::Escape);
    // Then: the list is back, and the model was not applied.
    assert!(harness.has_label(SEARCH_LABEL));
    assert!(!harness.has_label(EFFORT_HEADING));
    assert_eq!(thread_preference(&harness), None);
}

#[test]
fn clearing_selection_restores_automatic_routing() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness
        .state_mut()
        .set_thread_model_preference(preference("local", "model-b", None));
    harness.run();
    open_picker(&mut harness, "local / model-b");
    // When
    harness.click_label(AUTOMATIC_LABEL);
    harness.run();
    // Then
    assert_eq!(thread_preference(&harness), None);
}

#[test]
#[ignore = "writes picker PNG evidence using an offscreen GPU adapter"]
fn capture_model_picker_evidence() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness
        .state_mut()
        .set_thread_model_preference(preference("local", "model-a", None));
    harness.run();
    // When
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-picker.png"))
        .unwrap();
    open_picker(&mut harness, "local / model-a");
    // Then
    assert!(harness.has_label("remote / model-c"));
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-picker-open.png"))
        .unwrap();
}

#[test]
fn picker_disabled_without_profiles_offers_settings() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), false);
    harness.run();
    // When
    harness.click_label("⚙");
    harness.run();
    harness.click_label("Providers");
    harness.run();
    // Then
    assert!(harness.state().provider_settings().open);
    assert!(harness.has_label("Select model"));
}

const CREDENTIAL: &str =
    r#"credential = {type = "keyring", service = "evorch", account = "native-test"}"#;

fn native_workbench(
    root: &std::path::Path,
    kind: &str,
    model: &str,
    effort_levels: &str,
) -> HeadlessWorkbench<DemoSource> {
    let credential = if kind == "anthropic" { "" } else { CREDENTIAL };
    let mut harness = workbench_with_provider(
        root,
        true,
        &format!(
            r#"
[providers.codex]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = {{type = "keyring", service = "evorch", account = "codex-test"}}
models = [{{ id = "model-b", enabled = true, effort_levels = ["high"] }}]
default_model = "model-b"
[providers.a-native]
type = "{kind}"
{credential}
models = [{{ id = "{model}", enabled = true{effort_levels} }}]
default_model = "{model}"
"#
        ),
    );
    harness
        .state_mut()
        .set_thread_model_preference(preference("codex", "model-b", Some("high")));
    harness.run();
    harness
}

#[test]
fn effort_step_is_offered_for_anthropic_only_with_explicit_levels() {
    // Given: an Anthropic API profile whose model lists effort levels, and one without.
    for (levels, offered) in [(r#", effort_levels = ["low", "high"]"#, true), ("", false)] {
        let temp = tempfile::tempdir().unwrap();
        let mut harness = native_workbench(temp.path(), "anthropic", "selected-model", levels);
        // When: switching to it from a model with a stored effort.
        open_picker(&mut harness, "codex / model-b · high");
        harness.click_label("a-native / selected-model");
        harness.run();
        // Then: configured levels are offered (never the common defaults) and the
        // incompatible stored effort starts on Default; without levels there is no step.
        assert_eq!(harness.has_label(EFFORT_HEADING), offered, "{levels}");
        if offered {
            assert!(harness.has_label("low"));
            assert!(!harness.has_label("xhigh"));
            harness.click_label("low");
            harness.run();
        }
        let expected = preference("a-native", "selected-model", offered.then_some("low"));
        assert_eq!(thread_preference(&harness), expected, "{levels}");
    }
}

#[test]
fn cursor_never_offers_an_effort_step_and_clears_the_effort() {
    // Given: a Cursor profile, even with levels configured on the model.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = native_workbench(
        temp.path(),
        "cursor",
        "claude-opus-5-5-high",
        r#", effort_levels = ["high"]"#,
    );
    // When: switching to it from a model with a stored effort.
    open_picker(&mut harness, "codex / model-b · high");
    harness.click_label("a-native / claude-opus-5-5-high");
    harness.run();
    // Then: the model is applied at once with no effort.
    assert!(!harness.has_label(EFFORT_HEADING));
    assert!(!harness.has_label(SEARCH_LABEL));
    assert_eq!(
        thread_preference(&harness),
        preference("a-native", "claude-opus-5-5-high", None)
    );
}

#[test]
fn claude_subscription_keeps_compatible_effort_and_persists_changes() {
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_provider(
        temp.path(),
        true,
        r#"
[providers.a-claude]
type = "anthropic-subscription"
credential = {type = "keyring", service = "evorch", account = "claude-test"}
models = ["claude-sonnet-5-5"]
default_model = "claude-sonnet-5-5"
"#,
    );
    harness
        .state_mut()
        .set_thread_model_preference(preference("local", "model-b", Some("high")));
    harness.run();
    open_picker(&mut harness, "local / model-b · high");
    harness.click_label("a-claude / claude-sonnet-5-5");
    harness.run();
    // The stored effort is supported by the new model, so the step starts on it.
    assert!(harness.has_label(EFFORT_HEADING));
    assert!(harness.has_label("xhigh"));
    assert!(!harness.has_label("minimal"));
    assert!(!harness.has_label("none"));
    press(&mut harness, Key::Enter);
    let mut expected = preference("a-claude", "claude-sonnet-5-5", Some("high"));
    assert_eq!(thread_preference(&harness), expected);
    // Changing the effort of the current model reopens the step.
    open_picker(&mut harness, "a-claude / claude-sonnet-5-5 · high");
    harness.click_label("a-claude / claude-sonnet-5-5");
    harness.run();
    harness.click_label("max");
    harness.run();
    expected.as_mut().unwrap().reasoning_effort = Some("max".into());
    assert_eq!(thread_preference(&harness), expected);
    let restored = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert_eq!(restored.threads[0].model_preference, expected);
}
