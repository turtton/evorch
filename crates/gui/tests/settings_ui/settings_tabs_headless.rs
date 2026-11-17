//! The settings modal hosts Providers, Routing and Agent roles as tabs of one session.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use config::ChainEntryConfig;
use gui::{
    app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench,
    model::provider_settings::ProviderSettingsModel, panes::settings::SettingsTab,
};
use runtime::compose::SwitchableModel;

const CONFIG: &str = r#"
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
[routing.routes]
worker = [{ profile = "local", model = "base" }]
fast = [{ profile = "accelerated", model = "fast" }]
spare = [{ profile = "local", model = "base" }]
old = [{ profile = "local", model = "base" }]
"#;

fn load_options(root: &Path) -> config::LoadOptions {
    config::LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    }
}

fn saved(root: &Path) -> config::Config {
    config::Config::load(&load_options(root)).expect("saved config")
}

fn fixture(root: &Path, size: [f32; 2], extra: &str) -> HeadlessWorkbench<DemoSource> {
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
    std::fs::write(
        config::project_main_config_path(root),
        format!("{CONFIG}{extra}"),
    )
    .expect("fixture config");
    let context = gui::model::production::ProductionModel {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        load_options: load_options(root),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(root.join("credentials"))
                .expect("store"),
        ),
        bus: Arc::new(event_bus::EventBus::new(32)),
        env: Arc::new(routing::MapEnv::new(
            [("TEST_KEY".into(), "test-secret".into())].into(),
        )),
    };
    let model = Arc::new(SwitchableModel::new(context.reload().expect("runtime")));
    let providers = ProviderSettingsModel::seed_from_config(&saved(root));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_provider_settings_path(config::project_main_config_path(root))
        .with_provider_settings(providers)
        .with_production_model(context, model);
    HeadlessWorkbench::new(state, size)
}

fn wide(root: &Path) -> HeadlessWorkbench<DemoSource> {
    fixture(root, [1200.0, 900.0], "")
}

/// Steps until the save started by the last click has been received.
fn settle(harness: &mut HeadlessWorkbench<DemoSource>) {
    harness.step();
    while harness.state().routing_settings().is_saving()
        || harness.state().role_settings().is_saving()
    {
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
}

fn pick_tab(harness: &mut HeadlessWorkbench<DemoSource>, label: &str) {
    harness.click_label(label);
    harness.run();
}

fn route(name: &str) -> ChainEntryConfig {
    ChainEntryConfig::route(name)
}

#[test]
fn each_entry_opens_its_tab_and_reopening_only_switches_tabs() {
    // Given: a closed modal.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    assert_eq!(harness.state().settings_tab(), None);
    // When/Then: every public entry opens the tab that belongs to it.
    harness.state_mut().open_provider_settings();
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Providers));
    harness.state_mut().close_settings_modal();
    harness.state_mut().open_routing_settings();
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
    harness.state_mut().close_settings_modal();
    harness.state_mut().open_role_settings();
    assert_eq!(
        harness.state().settings_tab(),
        Some(SettingsTab::AgentRoles)
    );
    // When: entering another tab while open, the session and its drafts continue.
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("fast")];
    harness.state_mut().open_provider_settings();
    harness.state_mut().provider_settings_mut().edit("local");
    harness.state_mut().open_routing_settings();
    harness.state_mut().open_role_settings();
    // Then: nothing was re-seeded.
    assert_eq!(
        harness.state().role_settings().agents.explorer.head_route(),
        Some("fast")
    );
    harness.state_mut().open_provider_settings();
    assert!(harness.state().provider_editor_active());
    harness.run();
    assert!(harness.has_label("Provider settings"));
}

#[test]
fn tab_round_trip_keeps_drafts_and_saves_only_the_current_tab() {
    // Given: a role draft and a routing draft, both unsaved.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    harness.state_mut().open_role_settings();
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("fast")];
    harness.run();
    pick_tab(&mut harness, "Routing");
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
    harness
        .state_mut()
        .routing_settings_mut()
        .add_route("draft")
        .expect("draft route");
    harness.run();
    // When: going back and forth.
    pick_tab(&mut harness, "Agent roles");
    pick_tab(&mut harness, "Routing");
    pick_tab(&mut harness, "Agent roles");
    // Then: both drafts are still there and nothing was written.
    assert_eq!(
        harness.state().role_settings().agents.explorer.head_route(),
        Some("fast")
    );
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("draft")
    );
    assert!(saved(temp.path()).agents.explorer.models.is_empty());
    assert!(!saved(temp.path()).routing.routes.contains_key("draft"));
    // When: saving the roles tab.
    harness.click_label("Save role settings");
    settle(&mut harness);
    // Then: only the roles reached the disk and the modal stays open.
    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(
        harness.state().settings_tab(),
        Some(SettingsTab::AgentRoles)
    );
    assert_eq!(
        saved(temp.path()).agents.explorer.head_route(),
        Some("fast")
    );
    assert!(!saved(temp.path()).routing.routes.contains_key("draft"));
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("draft")
    );
    // When: saving the routing tab afterwards.
    pick_tab(&mut harness, "Routing");
    harness.click_label("Save routing");
    settle(&mut harness);
    // Then: routing is written too and the modal is still open on Routing.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
    assert!(saved(temp.path()).routing.routes.contains_key("draft"));
}

#[test]
fn closing_and_reopening_discards_unsaved_drafts_of_every_tab() {
    // Given: unsaved edits in two tabs.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    harness.state_mut().open_role_settings();
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("fast")];
    harness.state_mut().open_routing_settings();
    harness
        .state_mut()
        .routing_settings_mut()
        .add_route("draft")
        .expect("draft route");
    harness.run();
    assert!(harness.state().settings_has_unsaved_edits());
    // When: closing through the one Close button, then opening again.
    assert_eq!(harness.count_labels("Close"), 1);
    harness.click_label("Close");
    harness.run();
    assert_eq!(harness.state().settings_tab(), None);
    assert!(!harness.has_label("Routing settings"));
    harness.state_mut().open_role_settings();
    // Then: the new session starts from the saved config.
    assert!(!harness.state().settings_has_unsaved_edits());
    assert_eq!(
        harness.state().role_settings().agents.explorer.head_route(),
        None
    );
    harness.state_mut().open_routing_settings();
    assert!(
        !harness
            .state()
            .routing_settings()
            .routes
            .contains_key("draft")
    );
}

fn create_route_from_roles(harness: &mut HeadlessWorkbench<DemoSource>) {
    harness.state_mut().open_role_settings();
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("legacy")];
    harness.run();
    harness.click_label("Explorer");
    harness.run();
    harness.click_label("Create route");
    harness.run();
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
}

#[test]
fn role_draft_survives_creating_a_route_and_returns_after_the_routing_save() {
    // Given: a role draft naming a route that does not exist yet.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    create_route_from_roles(&mut harness);
    // Then: the role draft is kept untouched while the route is prepared in Routing.
    assert!(harness.state().routing_settings().origin_role_settings);
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("legacy")
    );
    assert_eq!(
        harness.state().role_settings().agents.explorer.head_route(),
        Some("legacy")
    );
    assert!(harness.state().role_settings().is_dirty());
    // When: saving the route.
    harness.click_label("Save routing");
    settle(&mut harness);
    // Then: back on roles with the draft intact and the route known to it.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    assert_eq!(
        harness.state().settings_tab(),
        Some(SettingsTab::AgentRoles)
    );
    assert!(
        harness
            .state()
            .role_settings()
            .routes
            .contains_key("legacy")
    );
    assert!(harness.state().role_settings().is_dirty());
    assert!(saved(temp.path()).routing.routes.contains_key("legacy"));
    assert!(saved(temp.path()).agents.explorer.models.is_empty());
    // When: saving the role draft.
    harness.click_label("Save role settings");
    settle(&mut harness);
    // Then: the binding reaches the disk.
    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(
        saved(temp.path()).agents.explorer.head_route(),
        Some("legacy")
    );
}

#[test]
fn a_failed_routing_save_stays_on_routing_with_both_drafts() {
    // Given: a role-originated route whose candidate is invalid.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    create_route_from_roles(&mut harness);
    harness
        .state_mut()
        .routing_settings_mut()
        .routes
        .get_mut("legacy")
        .expect("route")[0]
        .profile = "missing-profile".into();
    // When: saving.
    harness.click_label("Save routing");
    settle(&mut harness);
    // Then: Routing stays with the way back, and neither draft was lost.
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
    assert!(harness.state().routing_settings().origin_role_settings);
    assert!(
        harness
            .state()
            .routing_settings()
            .validation_error
            .is_some()
    );
    assert_eq!(
        harness.state().routing_settings().routes["legacy"][0].profile,
        "missing-profile"
    );
    assert_eq!(
        harness.state().role_settings().agents.explorer.head_route(),
        Some("legacy")
    );
    assert!(!saved(temp.path()).routing.routes.contains_key("legacy"));
}

#[test]
fn the_way_back_to_roles_is_dropped_by_direct_entry_or_manual_tab_moves() {
    // Given: a route created from roles.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    create_route_from_roles(&mut harness);
    assert!(harness.state().routing_settings().origin_role_settings);
    // When: opening Routing through its own entry.
    harness.state_mut().open_routing_settings();
    // Then: there is nothing to return to.
    assert!(!harness.state().routing_settings().origin_role_settings);
    // When: moving by hand and coming back, then saving.
    create_route_from_roles_again(&mut harness);
    pick_tab(&mut harness, "Agent roles");
    assert!(!harness.state().routing_settings().origin_role_settings);
    pick_tab(&mut harness, "Routing");
    harness.click_label("Save routing");
    settle(&mut harness);
    // Then: the save does not jump to roles.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
}

fn create_route_from_roles_again(harness: &mut HeadlessWorkbench<DemoSource>) {
    pick_tab(harness, "Agent roles");
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("legacy")];
    harness.run();
    if !harness.has_label("Create route") {
        harness.click_label("Explorer");
        harness.run();
    }
    harness.click_label("Create route");
    harness.run();
    assert!(harness.state().routing_settings().origin_role_settings);
}

#[test]
fn nothing_else_is_accepted_while_a_save_is_running() {
    // Given: a routing save that has started but not been received.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    harness.state_mut().open_routing_settings();
    harness
        .state_mut()
        .routing_settings_mut()
        .add_route("draft")
        .expect("draft route");
    harness.state_mut().submit_routing_settings();
    assert!(harness.state().routing_settings().is_saving());
    // When: other operations are attempted.
    harness
        .state_mut()
        .select_settings_tab(SettingsTab::AgentRoles);
    harness.state_mut().open_role_settings();
    harness.state_mut().close_settings_modal();
    harness.state_mut().submit_routing_settings();
    harness.state_mut().submit_role_settings();
    harness.state_mut().apply_routing_profile_action(
        gui::model::role_profiles::RoleProfileAction::Create("extra".into()),
    );
    // Then: they are ignored.
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
    assert!(!harness.state().role_settings().is_saving());
    settle(&mut harness);
    assert_eq!(harness.state().routing_settings().validation_error, None);
    assert!(saved(temp.path()).routing.routes.contains_key("draft"));
    assert!(
        !std::fs::read_to_string(config::project_main_config_path(temp.path()))
            .expect("config")
            .contains("extra")
    );
    // When: the save is over.
    harness
        .state_mut()
        .select_settings_tab(SettingsTab::AgentRoles);
    // Then: the tab can be switched again.
    assert_eq!(
        harness.state().settings_tab(),
        Some(SettingsTab::AgentRoles)
    );
}

#[test]
fn other_settings_dialogs_cannot_take_over_while_a_save_is_running() {
    // Given: a routing save that has started but not been received.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    harness.state_mut().open_routing_settings();
    harness
        .state_mut()
        .routing_settings_mut()
        .add_route("draft")
        .expect("draft route");
    harness.state_mut().submit_routing_settings();
    assert!(harness.state().routing_settings().is_saving());
    // When: every other settings entry is used.
    harness.state_mut().open_storage_settings();
    harness.state_mut().open_agent_features();
    harness.state_mut().open_sandbox_settings();
    harness.state_mut().open_self_improvement_settings();
    harness.state_mut().open_theme_settings();
    // Then: the session and its draft are kept.
    assert_eq!(harness.state().settings_tab(), Some(SettingsTab::Routing));
    assert!(harness.state().routing_settings().is_saving());
    settle(&mut harness);
    assert!(saved(temp.path()).routing.routes.contains_key("draft"));
}

#[test]
fn profile_changes_are_disabled_while_edits_are_unsaved() {
    // Given: an unsaved role edit.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    harness.state_mut().open_role_settings();
    harness.run();
    assert_eq!(harness.label_enabled("New profile"), Some(true));
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("fast")];
    harness.run();
    // Then: profile operations are disabled with one explanation, on both draft tabs.
    assert_eq!(harness.label_enabled("New profile"), Some(false));
    assert_eq!(
        harness.count_labels(gui::panes::role_profiles::PROFILE_LOCKED_NOTICE),
        1
    );
    pick_tab(&mut harness, "Routing");
    assert_eq!(harness.label_enabled("New profile"), Some(false));
    assert_eq!(
        harness.count_labels(gui::panes::role_profiles::PROFILE_LOCKED_NOTICE),
        1
    );
}

#[test]
fn a_routing_rename_is_applied_to_the_role_draft_and_not_written_back_by_roles() {
    // Given: a role draft that uses a route, and a rename of that route in Routing.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = wide(temp.path());
    harness.state_mut().open_role_settings();
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .worker
        .base
        .models = vec![route("old"), route("fast")];
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .models = vec![route("spare")];
    harness.state_mut().open_routing_settings();
    harness
        .state_mut()
        .routing_settings_mut()
        .rename_route("old", "new")
        .expect("rename");
    harness.run();
    // When: saving the rename.
    harness.click_label("Save routing");
    settle(&mut harness);
    // Then: the draft follows the rename and keeps every other value.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let drafted = &harness.state().role_settings().agents;
    assert_eq!(drafted.worker.base.models, [route("new"), route("fast")]);
    assert_eq!(drafted.explorer.models, [route("spare")]);
    // When: saving the roles afterwards.
    harness.state_mut().open_role_settings();
    harness.run();
    harness.click_label("Save role settings");
    settle(&mut harness);
    // Then: the old name is not written back.
    assert_eq!(harness.state().role_settings().error, None);
    let disk = saved(temp.path());
    assert!(disk.routing.routes.contains_key("new"));
    assert!(!disk.routing.routes.contains_key("old"));
    assert_eq!(
        disk.agents.worker.base.models,
        [route("new"), route("fast")]
    );
    assert_eq!(disk.agents.explorer.models, [route("spare")]);
    assert!(
        config::types::agents::explicit_refs(&disk.agents)
            .iter()
            .all(|(_, name)| name != "old")
    );
}

#[test]
fn every_control_stays_reachable_in_a_narrow_window() {
    // Given: a small window and a worker chain with fallbacks.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = fixture(
        temp.path(),
        [480.0, 640.0],
        "[agents.worker]\nmodels = [{ route = 'worker' }, { route = 'fast' }, { route = 'spare' }]\n",
    );
    let screen = harness.screen_rect();
    let inside = |harness: &HeadlessWorkbench<DemoSource>, label: &str| {
        let rects = harness.label_rects(label);
        assert!(!rects.is_empty(), "{label} is missing");
        for rect in rects {
            assert!(
                screen.contains_rect(rect),
                "{label} {rect:?} leaves the window {screen:?} ({:?})",
                harness.state().settings_tab()
            );
        }
    };
    for (tab, save) in [
        (SettingsTab::Providers, None),
        (SettingsTab::Routing, Some("Save routing")),
        (SettingsTab::AgentRoles, Some("Save role settings")),
    ] {
        harness.state_mut().open_settings_tab(tab, None);
        harness.run();
        // Then: the tab strip, the close button and the tab's save are all on screen.
        for label in ["Providers", "Routing", "Agent roles", "Close"] {
            inside(&harness, label);
        }
        if let Some(save) = save {
            inside(&harness, save);
        }
    }
    // When: a role chain with fallbacks is open.
    harness.click_label("Worker");
    harness.run();
    // Then: the icon buttons are reachable by name and on screen.
    for label in [
        "Move Role route fallback 1 up",
        "Move Role route fallback 1 down",
        "Remove Role route fallback 1",
        "Add Role route fallback",
    ] {
        harness.scroll_label_into_view(label);
        harness.run();
        inside(&harness, label);
    }
}

#[test]
fn fallback_icon_buttons_report_their_disabled_state_by_name() {
    // Given: a worker chain with the head and one fallback, and routes left to add.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = fixture(
        temp.path(),
        [1200.0, 900.0],
        "[agents.worker]\nmodels = [{ route = 'worker' }, { route = 'fast' }]\n",
    );
    harness.state_mut().open_role_settings();
    harness.run();
    harness.click_label("Worker");
    harness.run();
    // Then: the last fallback cannot move down, everything else can be used.
    assert_eq!(
        harness.label_enabled("Move Role route fallback 1 up"),
        Some(true)
    );
    assert_eq!(
        harness.label_enabled("Move Role route fallback 1 down"),
        Some(false)
    );
    assert_eq!(
        harness.label_enabled("Remove Role route fallback 1"),
        Some(true)
    );
    assert_eq!(harness.label_enabled("Add Role route fallback"), Some(true));
    // When: adding a fallback through the icon button.
    harness.click_label("Add Role route fallback");
    harness.run();
    // Then: the new entry exists and the earlier one can now move down.
    assert_eq!(
        harness
            .state()
            .role_settings()
            .agents
            .worker
            .base
            .models
            .len(),
        3
    );
    assert_eq!(
        harness.label_enabled("Move Role route fallback 1 down"),
        Some(true)
    );
    assert_eq!(
        harness.label_enabled("Move Role route fallback 2 down"),
        Some(false)
    );
    // When: moving the first fallback down and removing the other one.
    harness.click_label("Move Role route fallback 1 down");
    harness.run();
    let order: Vec<_> = harness
        .state()
        .role_settings()
        .agents
        .worker
        .base
        .models
        .iter()
        .map(|entry| entry.route.clone())
        .collect();
    harness.click_label("Remove Role route fallback 2");
    harness.run();
    // Then: the operations acted on the entries they name.
    assert_eq!(order[0], "worker");
    assert_eq!(order[2], "fast");
    assert_eq!(
        harness
            .state()
            .role_settings()
            .agents
            .worker
            .base
            .models
            .len(),
        2
    );
}

#[test]
fn adding_a_fallback_is_disabled_when_every_route_is_already_used() {
    // Given: a chain that already names every route.
    let temp = tempfile::tempdir().expect("temp");
    let mut harness = fixture(
        temp.path(),
        [1200.0, 900.0],
        "[agents.worker]\nmodels = [{ route = 'worker' }, { route = 'fast' }, { route = 'spare' }, { route = 'old' }]\n",
    );
    harness.state_mut().open_role_settings();
    harness.run();
    harness.click_label("Worker");
    harness.run();
    // Then: the add button is disabled for assistive technology and does nothing.
    assert_eq!(
        harness.label_enabled("Add Role route fallback"),
        Some(false)
    );
    harness.click_label("Add Role route fallback");
    harness.run();
    assert_eq!(
        harness
            .state()
            .role_settings()
            .agents
            .worker
            .base
            .models
            .len(),
        4
    );
}

mod auto_added_routes {
    use super::*;

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
[routing.routes]
"moonshotai/kimi-k3" = [{ profile = "vivgrid" }]
[agents.roles.planner]
models = [{ route = "moonshotai/kimi-k3" }]
"#;

    async fn catalog() -> catalog::ModelCatalog {
        let dir = tempfile::tempdir().unwrap();
        let model = |id: &str| serde_json::json!({"id": id, "canonical_model_id": POOL});
        let api = serde_json::json!({
            "vivgrid": {"models": {"kimi-k3": model("kimi-k3")}},
            "neuralwatt": {"models": {"kimi-k3": model("kimi-k3")}},
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

    async fn workbench(root: &Path) -> HeadlessWorkbench<DemoSource> {
        std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).unwrap();
        std::fs::write(config::project_main_config_path(root), PROVIDERS).unwrap();
        let config = config::Config::load(&load_options(root)).unwrap();
        let mut settings = ProviderSettingsModel::seed_from_config(&config);
        settings.catalog.catalog = Some(Arc::new(catalog().await));
        let state =
            WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                .unwrap()
                .with_settings_load_options(load_options(root))
                .with_provider_settings_path(config::project_main_config_path(root))
                .with_provider_settings(settings);
        HeadlessWorkbench::new(state, [1200.0, 900.0])
    }

    fn pool(root: &Path) -> usize {
        saved(root).routing.routes[POOL].len()
    }

    fn save_provider(harness: &mut HeadlessWorkbench<DemoSource>, profile: &str) {
        harness.state_mut().open_provider_settings();
        harness.state_mut().provider_settings_mut().edit(profile);
        harness.run();
        harness.click_label("Save");
        harness.wait_provider_save(Duration::from_secs(10));
        harness.run();
        assert_eq!(harness.state().provider_settings().error, None);
    }

    // Given: a Routing draft with an unsaved edit
    // When: a provider save adds an automatic fallback to a saved route
    // Then: the draft is kept, saving it is disabled with the way out, and it never overwrites
    #[tokio::test]
    async fn an_edited_routing_draft_is_kept_but_cannot_overwrite_the_automatic_route() {
        let temp = tempfile::tempdir().unwrap();
        let mut harness = workbench(temp.path()).await;
        harness.state_mut().open_routing_settings();
        harness
            .state_mut()
            .routing_settings_mut()
            .add_route("draft")
            .unwrap();
        save_provider(&mut harness, "neuralwatt");
        assert_eq!(pool(temp.path()), 2);
        harness
            .state_mut()
            .select_settings_tab(SettingsTab::Routing);
        harness.run();
        assert!(
            harness
                .state()
                .routing_settings()
                .routes
                .contains_key("draft")
        );
        assert!(harness.state().routing_settings().conflict.is_some());
        assert_eq!(harness.label_enabled("Save routing"), Some(false));
        harness.state_mut().submit_routing_settings();
        assert!(!harness.state().routing_settings().is_saving());
        assert_eq!(pool(temp.path()), 2);
        assert!(!saved(temp.path()).routing.routes.contains_key("draft"));
        // When: closing and opening again.
        harness.state_mut().close_settings_modal();
        harness.state_mut().open_routing_settings();
        harness.run();
        // Then: it starts from the saved routing, automatic candidate included.
        assert!(harness.state().routing_settings().conflict.is_none());
        assert_eq!(harness.state().routing_settings().routes[POOL].len(), 2);
        assert_eq!(harness.label_enabled("Save routing"), Some(true));
    }

    // Given: a Routing tab without edits
    // When: a provider save adds an automatic fallback
    // Then: the tab shows the written route
    #[tokio::test]
    async fn an_unedited_routing_tab_picks_up_the_automatic_route() {
        let temp = tempfile::tempdir().unwrap();
        let mut harness = workbench(temp.path()).await;
        harness.state_mut().open_routing_settings();
        save_provider(&mut harness, "neuralwatt");
        harness
            .state_mut()
            .select_settings_tab(SettingsTab::Routing);
        assert_eq!(harness.state().routing_settings().routes[POOL].len(), 2);
        assert!(harness.state().routing_settings().conflict.is_none());
        assert!(!harness.state().routing_settings().is_dirty());
    }

    // Given: an edited Routing draft
    // When: a provider save leaves the saved routing as it was
    // Then: the draft stays and can still be saved
    #[tokio::test]
    async fn an_edited_draft_survives_a_provider_save_that_changes_no_route() {
        let temp = tempfile::tempdir().unwrap();
        let mut harness = workbench(temp.path()).await;
        harness.state_mut().open_routing_settings();
        harness
            .state_mut()
            .routing_settings_mut()
            .add_route("draft")
            .unwrap();
        save_provider(&mut harness, "vivgrid");
        harness
            .state_mut()
            .select_settings_tab(SettingsTab::Routing);
        harness.run();
        assert!(
            harness
                .state()
                .routing_settings()
                .routes
                .contains_key("draft")
        );
        assert!(harness.state().routing_settings().conflict.is_none());
        assert_eq!(harness.label_enabled("Save routing"), Some(true));
    }
}
