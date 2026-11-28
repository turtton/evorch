//! GUI model contracts for model kinds and the image generator chain (ADR 0038).

use config::{ChainPurpose, Config, ModelKind};
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
