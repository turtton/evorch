//! GUI model contracts for model kinds and the image generator chain (ADR 0038).

use config::{ChainPurpose, Config, ModelKind};
use gui::model::provider_settings::OpenAiEditorModel;
use gui::model::role_settings::RoleSettingsModel;
use gui::model::routing_settings::RoutingSettingsModel;

const CONFIG: &str = r#"
[providers.openai]
type = "openai"
api_protocol = "openai-responses"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["gpt-5", "gpt-image-1", "dall-e-3"]
default_model = "gpt-5"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["local-chat", "gpt-image-1"]
default_model = "local-chat"
[routing.routes]
chat = [{ profile = "openai" }]
image = [{ profile = "openai", model = "gpt-image-1" }]
local-image = [{ profile = "local", model = "gpt-image-1" }]
mixed = [{ profile = "openai" }, { profile = "openai", model = "dall-e-3" }]
empty = []
"#;

fn config() -> Config {
    toml::from_str(CONFIG).expect("fixture config")
}

#[test]
fn added_models_record_their_inferred_kind_but_loaded_ones_stay_untouched() {
    // Given: a profile loaded from a config that never wrote `kind`.
    let config = config();
    let mut editor = OpenAiEditorModel::from_profile("openai", &config.providers["openai"]);
    assert!(
        editor
            .to_input()
            .models
            .iter()
            .all(|model| model.kind.is_none())
    );
    // When: models are typed in and picked from the fetched list.
    editor.add_model("gpt-image-2").expect("add image model");
    editor.add_model("gpt-5-mini").expect("add chat model");
    editor.available_models = Some(vec!["chatgpt-image-latest".into(), "o4".into()]);
    editor.selection_toggle("chatgpt-image-latest");
    editor.selection_toggle("o4");
    editor.apply_fetched_selection();
    // Then: only the added models carry the inferred kind explicitly.
    let kinds: Vec<_> = editor
        .to_input()
        .models
        .iter()
        .map(|model| (model.id.clone(), model.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("gpt-5".to_owned(), None),
            ("gpt-image-1".to_owned(), None),
            ("dall-e-3".to_owned(), None),
            ("gpt-image-2".to_owned(), Some(ModelKind::ImageGeneration)),
            ("gpt-5-mini".to_owned(), Some(ModelKind::Chat)),
            (
                "chatgpt-image-latest".to_owned(),
                Some(ModelKind::ImageGeneration)
            ),
            ("o4".to_owned(), Some(ModelKind::Chat)),
        ]
    );
}

#[test]
fn image_models_are_never_the_default_model() {
    // Given: an editor whose first enabled model is an image model.
    let mut editor = OpenAiEditorModel::default();
    editor.add_model("gpt-image-1").expect("add");
    // Then: no default is chosen until a chat model exists.
    assert_eq!(editor.default_model, "");
    assert_eq!(
        editor.validation_error.as_deref(),
        Some("at least one enabled chat model required for the default model")
    );
    editor.add_model("gpt-5").expect("add");
    assert_eq!(editor.default_model, "gpt-5");
    assert_eq!(editor.validation_error, None);
    assert_eq!(editor.candidate_models(), ["gpt-5"]);
    // When: the user marks the image model as a chat model, then the default as an image model.
    editor.set_model_kind(0, ModelKind::Chat).expect("set kind");
    assert_eq!(editor.candidate_models(), ["gpt-image-1", "gpt-5"]);
    editor
        .set_model_kind(1, ModelKind::ImageGeneration)
        .expect("set kind");
    // Then: the default moves to the remaining chat model.
    assert_eq!(editor.default_model, "gpt-image-1");
    assert_eq!(editor.candidate_models(), ["gpt-image-1"]);
}

#[test]
fn chains_offer_only_routes_of_their_kind() {
    // Given: chat, image, mixed, empty and unsupported-provider image routes, plus a
    // chain naming a route that does not exist yet.
    let mut config = config();
    config.agents.explorer.models = vec![config::ChainEntryConfig::route("planned")];
    let model = RoleSettingsModel::seed_from_config(&config);
    // When
    let conversation = model.chain_routes(ChainPurpose::Conversation);
    let image = model.chain_routes(ChainPurpose::ImageGeneration);
    // Then: empty and undefined routes fit both; mixed routes fit neither.
    assert_eq!(conversation.heads, ["chat", "empty", "planned"]);
    assert_eq!(
        conversation.existing,
        ["chat", "empty"].map(String::from).into()
    );
    assert_eq!(
        conversation.misfits,
        ["image", "local-image", "mixed"].map(String::from).into()
    );
    assert_eq!(image.heads, ["empty", "image", "planned"]);
    assert_eq!(
        image.misfits,
        ["chat", "local-image", "mixed"].map(String::from).into()
    );
}

#[test]
fn role_settings_refuse_chains_of_the_wrong_kind_before_saving() {
    for (address, route) in [
        ("agents.worker.models", "image"),
        ("agents.roles.compactor.models", "image"),
        ("agents.worker.categories.quick.models", "image"),
        ("agents.roles.image_generator.models", "chat"),
        ("agents.roles.image_generator.models", "local-image"),
    ] {
        // Given
        let mut model = RoleSettingsModel::seed_from_config(&config());
        let chain = config::types::agents::chain_at_mut(
            &mut model.agents,
            address
                .trim_start_matches("agents.")
                .trim_end_matches(".models"),
        )
        .expect("chain address");
        *chain = vec![config::ChainEntryConfig::route(route)];
        // When
        let error = model.validate().expect_err("kind mismatch");
        // Then
        assert!(
            matches!(&error, config::ConfigError::InvalidField { path, message }
                if path == address && message.contains(&format!("`{route}`"))),
            "{address}: {error}"
        );
    }
    let mut model = RoleSettingsModel::seed_from_config(&config());
    model.agents.roles.image_generator.models = vec![config::ChainEntryConfig::route("image")];
    model.agents.worker.base.models = vec![config::ChainEntryConfig::route("chat")];
    model.validate().expect("matching kinds are accepted");
}

#[test]
fn route_candidates_offer_only_models_of_the_route_kind() {
    // Given
    let mut config = config();
    config.agents.roles.image_generator.models = vec![config::ChainEntryConfig::route("image")];
    let mut model = RoutingSettingsModel::seed_from_config(&config);
    // When / Then: a candidate of an image route sees image models only, without the
    // chat profile default; a lone candidate sees every model.
    model
        .routes
        .get_mut("image")
        .expect("route")
        .push(config::RouteCandidateConfig {
            profile: "openai".into(),
            model: Some("dall-e-3".into()),
            auto: false,
        });
    let choices = model.model_choices("image", 1);
    assert_eq!(choices.models, ["gpt-image-1", "dall-e-3"]);
    assert!(!choices.profile_default);
    let lone = model.model_choices("chat", 0);
    assert_eq!(lone.models, ["gpt-5", "gpt-image-1", "dall-e-3"]);
    assert!(lone.profile_default);
    // Adding a candidate to an image route starts on an image model of a profile that
    // can generate images.
    assert_eq!(
        model.new_candidate("image"),
        config::RouteCandidateConfig {
            profile: "openai".into(),
            model: Some("gpt-image-1".into()),
            auto: false,
        }
    );
    assert_eq!(model.new_candidate("chat").model, None);
    // Moving a candidate to a profile keeps the route's kind.
    model.routes.get_mut("image").expect("route")[1].model = None;
    assert!(model.change_candidate_profile("image", 1, "local"));
    assert_eq!(
        model.routes["image"][1].model.as_deref(),
        Some("gpt-image-1")
    );
}

#[test]
fn changing_to_a_chat_only_profile_never_inserts_an_incompatible_default() {
    let mut config = config();
    config
        .providers
        .get_mut("local")
        .unwrap()
        .models
        .retain(|model| model.effective_kind() == ModelKind::Chat);
    for count in [1, 2] {
        let mut model = RoutingSettingsModel::seed_from_config(&config);
        let original = model.routes["image"][0].clone();
        model
            .routes
            .insert("image".into(), vec![original.clone(); count]);
        assert!(!model.change_candidate_profile("image", count - 1, "local"));
        assert_eq!(model.routes["image"][count - 1], original);
        assert!(!model.change_candidate_profile("image", count - 1, "unknown"));
        assert_eq!(model.routes["image"][count - 1], original);
    }
}

#[test]
fn routing_settings_refuse_mixed_routes_and_misused_routes_before_saving() {
    // Given: the fixture's mixed route (the empty route is refused for another reason).
    let mut config = config();
    config.routing.routes.remove("empty");
    let model = RoutingSettingsModel::seed_from_config(&config);
    // When / Then
    let error = model.validated_routing().expect_err("mixed route");
    assert!(
        matches!(&error, config::ConfigError::InvalidField { path, .. } if path == "routing.routes.mixed"),
        "{error}"
    );
    // Given: the worker uses `chat`, which is edited to an image model.
    config.routing.routes.remove("mixed");
    config.agents.worker.base.models = vec![config::ChainEntryConfig::route("chat")];
    let mut model = RoutingSettingsModel::seed_from_config(&config);
    model.validated_routing().expect("consistent routes save");
    model.routes.get_mut("chat").expect("route")[0].model = Some("gpt-image-1".into());
    let error = model
        .validated_routing()
        .expect_err("worker would use an image model");
    assert!(
        matches!(&error, config::ConfigError::InvalidField { path, .. } if path == "agents.worker.models"),
        "{error}"
    );
}

#[test]
fn a_route_created_for_the_image_generator_starts_on_an_image_model() {
    // Given: the image generator names a route that does not exist yet.
    let mut config = config();
    config.routing.routes.remove("mixed");
    config.routing.routes.remove("empty");
    config.agents.roles.image_generator.models = vec![config::ChainEntryConfig::route("new-image")];
    // When: role settings open routing settings to create it.
    let model = RoutingSettingsModel::seed_from_config_prefill(&config, "new-image");
    // Then: its first candidate is an image model of a profile that can generate images.
    assert_eq!(
        model.routes["new-image"],
        [config::RouteCandidateConfig {
            profile: "openai".into(),
            model: Some("gpt-image-1".into()),
            auto: false,
        }]
    );
    model
        .validated_routing()
        .expect("the new route fits its chain");
}
