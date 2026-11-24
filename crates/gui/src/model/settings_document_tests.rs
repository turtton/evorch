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

// Given: a document with a comment, restart targets, a provider, a route and a binding
// When: the routing draft adds a candidate-less change (a new route) and is turned into text
// Then: the text keeps everything else, and reads back with the new route
#[test]
fn a_routing_save_replaces_only_the_routing() {
    let document = SettingsDocument::parse(DOCUMENT).unwrap();
    let mut routing = document.seed_routing(None);
    assert_eq!(routing.profiles.selected, "default");
    assert_eq!(routing.route_users["fast"], vec!["explorer"]);
    routing.add_route("slow").unwrap();
    let text = document.routing_text(&routing).unwrap();
    for kept in [
        "# the person's note",
        "idle_grace_secs = 123",
        "/local/core",
        "[providers.local]",
    ] {
        assert!(text.contains(kept), "{kept} in {text}");
    }
    let saved = SettingsDocument::parse(&text).unwrap();
    assert!(saved.seed_routing(None).routes.contains_key("slow"));
    assert!(!saved.routes_unchanged(&routing));
    assert!(document.routes_unchanged(&routing));
}

// Given: the route `fast` that the explorer binding uses
// When: the route is renamed to `quick` and saved
// Then: the binding in the file follows the rename, and carrying the save into an open roles
//       draft renames its binding too
#[test]
fn a_renamed_route_takes_the_file_bindings_with_it() {
    let document = SettingsDocument::parse(DOCUMENT).unwrap();
    let mut routing = document.seed_routing(None);
    routing.rename_route("fast", "quick").unwrap();
    let text = document.routing_text(&routing).unwrap();
    let saved = SettingsDocument::parse(&text).unwrap();
    let roles = saved.seed_roles(None);
    assert_eq!(
        roles.agents.explorer.models,
        vec![config::ChainEntryConfig::route("quick")]
    );

    let mut open_roles = document.seed_roles(None);
    saved.carry_routing_save(&mut routing, Some(&mut open_roles));
    assert!(routing.routes.contains_key("quick"));
    assert!(!routing.is_dirty());
    assert_eq!(
        open_roles.agents.explorer.models,
        vec![config::ChainEntryConfig::route("quick")]
    );
    assert!(!open_roles.is_dirty());
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
