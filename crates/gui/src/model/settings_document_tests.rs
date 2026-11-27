use super::*;

const DOCUMENT: &str = r#"version = 3
# the person's note
[core]
idle_grace_secs = 123

[core.restart.targets.next]
path = "/local/core"

[providers.local]
type = "openai-compatible"
base_url = "http://localhost:1/v1"
api_key_env = "KEY"
models = ["m"]
default_model = "m"

[routing.routes]
fast = [{ profile = "local" }]

[agents.explorer]
models = [{ route = "fast" }]
"#;

const TWO_ROUTES: &str = r#"version = 3
# the person's note
[providers.local]
type = "openai-compatible"
base_url = "http://localhost:1/v1"
api_key_env = "KEY"
models = ["m", "n"]
default_model = "m"

[routing.routes]
fast = [{ profile = "local" }]
slow = [{ profile = "local", model = "n" }]

[routing.cooldown]
quota_secs = 7

[agents.explorer]
models = [{ route = "fast" }]

[agents.worker]
models = [{ route = "slow", reasoning_effort = "high" }]
generation = { temperature = 0.2 }

[agents.worker.categories.deep]
models = [{ route = "fast" }]
preset = "careful"
generation = { max_tokens = 4000 }
"#;

fn saved_routes(text: &str) -> Vec<String> {
    let saved = SettingsDocument::parse(text).unwrap();
    saved.seed_routing(None).routes.keys().cloned().collect()
}

// Given: two routes with unsaved edits, of which `fast` is selected
// When: the text for `fast` is made
// Then: only `fast` is written: `slow` stays as the file has it, and the cooldown, comments
//       and bindings are kept
#[test]
fn a_route_text_writes_the_selected_route_and_nothing_else_of_the_drafts() {
    let document = SettingsDocument::parse(TWO_ROUTES).unwrap();
    let mut routing = document.seed_routing(None);
    routing.routes.get_mut("fast").unwrap()[0].model = Some("n".into());
    routing.routes.get_mut("slow").unwrap().clear();
    assert_eq!(routing.dirty_routes(), vec!["fast", "slow"]);
    let text = document.route_text(&routing, "fast").unwrap();
    assert!(text.contains("# the person's note"), "{text}");
    let saved = SettingsDocument::parse(&text).unwrap().seed_routing(None);
    assert_eq!(saved.routes["fast"][0].model.as_deref(), Some("n"));
    assert_eq!(
        saved.routes["slow"],
        document.seed_routing(None).routes["slow"]
    );
    assert_eq!(saved.cooldown.quota_secs, 7);
    // The other one does not validate, and says so only when it is the one being saved.
    assert!(document.route_text(&routing, "slow").is_err());
}

// Given: a new route and a renamed one
// When: each is saved in turn and its document adopted
// Then: the rename and the bindings that name it are one text, the other draft is still
//       unsaved and saves against the new file, and the roles draft carries the rename
#[test]
fn a_rename_and_its_bindings_are_one_text_and_the_other_drafts_survive() {
    let document = SettingsDocument::parse(TWO_ROUTES).unwrap();
    let mut routing = document.seed_routing(None);
    let mut roles = document.seed_roles(None);
    roles.agents.explorer.generation.max_tokens = Some(9);
    routing.rename_route("fast", "quick").unwrap();
    let key = routing.add_draft_route();
    assert_eq!(key, "new-route");
    routing.rename_route(&key, "extra").unwrap();

    let text = document.route_text(&routing, "fast").unwrap();
    let saved = SettingsDocument::parse(&text).unwrap();
    assert_eq!(saved_routes(&text), vec!["quick", "slow"]);
    let view = saved.seed_roles(None);
    assert_eq!(
        view.agents.explorer.models,
        vec![config::ChainEntryConfig::route("quick")]
    );
    assert_eq!(
        view.agents.worker.categories["deep"].models,
        vec![config::ChainEntryConfig::route("quick")]
    );

    saved.adopt_routing(&mut routing, Some(("fast", "quick")));
    let renames: std::collections::BTreeMap<_, _> =
        [("fast".to_owned(), "quick".to_owned())].into();
    saved.adopt_roles(&mut roles, &renames, None);
    assert_eq!(routing.dirty_routes(), vec!["new-route"]);
    assert!(routing.conflicted.is_empty());
    assert!(routing.routes.contains_key("quick") && !routing.routes.contains_key("fast"));
    assert_eq!(
        roles.agents.explorer.models,
        vec![config::ChainEntryConfig::route("quick")]
    );
    assert_eq!(roles.agents.explorer.generation.max_tokens, Some(9));
    assert!(roles.conflicted.is_empty());
    let next = saved.route_text(&routing, "new-route").unwrap();
    assert_eq!(saved_routes(&next), vec!["extra", "quick", "slow"]);
}

// Given: a route with unsaved edits and one without
// When: another writer changes both, and the document is adopted
// Then: the one without edits follows the file, the other keeps its draft and is flagged
//       conflicted, which Discard clears; it is not saveable until then
#[test]
fn a_route_changed_elsewhere_under_unsaved_edits_is_flagged_and_discarded_to_resolve() {
    let document = SettingsDocument::parse(TWO_ROUTES).unwrap();
    let mut routing = document.seed_routing(None);
    routing.routes.get_mut("fast").unwrap()[0].auto = true;
    let elsewhere = TWO_ROUTES
        .replace(
            "fast = [{ profile = \"local\" }]",
            "fast = [{ profile = \"local\", model = \"n\" }]",
        )
        .replace(
            "model = \"n\" }]\n\n[routing.cooldown",
            "model = \"m\" }]\n\n[routing.cooldown",
        );
    let newer = SettingsDocument::parse(&elsewhere).unwrap();
    newer.adopt_routing(&mut routing, None);
    assert_eq!(routing.routes["slow"][0].model.as_deref(), Some("m"));
    assert!(routing.conflicted.contains("fast"));
    assert!(routing.routes["fast"][0].auto);
    routing.discard_route("fast");
    assert!(routing.conflicted.is_empty());
    assert_eq!(routing.routes["fast"][0].model.as_deref(), Some("n"));
    assert!(routing.dirty_routes().is_empty());
}

// Given: two roles with unsaved edits, the Worker selected, and categories and generation set
// When: the text for the Worker is made and adopted
// Then: the Worker's chain, generation and category override are written, the other role's
//       edit is not, and it stays a draft
#[test]
fn a_role_text_writes_the_selected_role_with_its_categories_only() {
    let document = SettingsDocument::parse(TWO_ROUTES).unwrap();
    let mut roles = document.seed_roles(None);
    let worker = ROLES.iter().find(|role| role.key == "worker").unwrap();
    roles.agents.worker.base.generation.temperature = Some(0.5);
    roles
        .agents
        .worker
        .categories
        .get_mut("deep")
        .unwrap()
        .preset = None;
    roles.agents.explorer.generation.max_tokens = Some(9);
    assert_eq!(roles.dirty_roles().len(), 2);
    let text = document.role_text(&roles, worker).unwrap();
    let saved = SettingsDocument::parse(&text).unwrap();
    let view = saved.seed_roles(None);
    assert_eq!(view.agents.worker.base.generation.temperature, Some(0.5));
    assert_eq!(
        view.agents.worker.base.models[0]
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
    assert_eq!(view.agents.worker.categories["deep"].preset, None);
    assert_eq!(
        view.agents.worker.categories["deep"].generation.max_tokens,
        Some(4000)
    );
    assert_eq!(view.agents.explorer.generation.max_tokens, None);
    saved.adopt_roles(&mut roles, &Default::default(), Some("worker"));
    assert_eq!(
        roles
            .dirty_roles()
            .iter()
            .map(|role| role.key)
            .collect::<Vec<_>>(),
        vec!["explorer"]
    );
    assert!(roles.conflicted.is_empty());
}

// Given: a saved route
// When: its removal text is made
// Then: only that route is gone, and the binding that named it is kept
#[test]
fn a_removal_text_deletes_only_that_route() {
    let document = SettingsDocument::parse(TWO_ROUTES).unwrap();
    let routing = document.seed_routing(None);
    let text = document.route_removed_text(&routing, "slow").unwrap();
    assert_eq!(saved_routes(&text), vec!["fast"]);
    let roles = SettingsDocument::parse(&text).unwrap().seed_roles(None);
    assert_eq!(roles.agents.worker.base.models[0].route, "slow");
}

// Given: a document / When: a profile is copied and then deleted / Then: both texts read back
#[test]
fn profiles_are_created_from_the_selected_one_and_deleted() {
    let document = SettingsDocument::parse(DOCUMENT).unwrap();
    let created =
        SettingsDocument::parse(&document.new_profile_text("default", "cheap").unwrap()).unwrap();
    let routing = created.seed_routing(Some("cheap"));
    assert_eq!(routing.profiles.selected, "cheap");
    assert!(routing.routes.contains_key("fast"));
    let deleted = SettingsDocument::parse(&created.without_profile_text("cheap").unwrap()).unwrap();
    assert_eq!(
        deleted.seed_routing(Some("cheap")).profiles.selected,
        "default"
    );
    assert!(document.new_profile_text("gone", "cheap").is_err());
}

#[test]
fn an_unreadable_document_is_refused() {
    assert!(SettingsDocument::parse("version = [").is_err());
}

#[test]
fn new_provider_names_skip_the_taken_ones() {
    let taken = ["kimi", "kimi-2"];
    let defaults = new_provider_defaults(NewProvider::Kimi, |name| taken.contains(&name));
    assert_eq!(defaults.name, "kimi-3");
    assert_eq!(
        defaults.base_url,
        config::types::provider::KIMI_DEFAULT_BASE_URL
    );
    assert_eq!(
        defaults.default_model,
        config::types::provider::KIMI_DEFAULT_MODEL
    );
    let open = new_provider_defaults(NewProvider::OpenaiCompatible, |_| false);
    assert_eq!(open.name, "openai-compat");
    assert!(open.base_url.is_empty() && open.models.is_empty());
}
