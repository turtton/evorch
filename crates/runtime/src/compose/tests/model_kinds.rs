//! Conversation and summary chains send only to chat models (ADR 0038).

use super::fallback::fixture_with;
use super::*;
use crate::ModelPreference;
use config::ModelKind;

fn text(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn invocation(
    purpose: event_bus::RequestPurpose,
    preference: Option<ModelPreference>,
) -> AgentInvocationContext {
    AgentInvocationContext {
        attempts: None,
        run_id: "run".into(),
        purpose,
        model_preference: preference,
        ..Default::default()
    }
}

async fn send(
    model: &RoutedModel,
    purpose: event_bus::RequestPurpose,
    preference: Option<ModelPreference>,
) -> Result<ChatResponse, RuntimeError> {
    model
        .complete(
            &invocation(purpose, preference),
            Role::Worker,
            &[text("work")],
            &[],
        )
        .await
}

/// Registers `model` on `profile` with an explicit kind, as a saved config would.
fn register(model: &mut RoutedModel, profile: &str, id: &str, kind: ModelKind) {
    model.provider_configs.insert(
        profile.into(),
        config::ProviderProfileConfig {
            models: vec![config::ModelEntryConfig {
                kind: Some(kind),
                ..config::ModelEntryConfig::enabled(id)
            }],
            default_model: id.into(),
            ..Default::default()
        },
    );
}

fn mismatch(origin: &str, profile: &str, model: &str) -> RuntimeError {
    RuntimeError::ModelKindMismatch {
        origin: origin.into(),
        profile: profile.into(),
        model: model.into(),
        kind: ModelKind::ImageGeneration,
    }
}

#[tokio::test]
async fn a_conversation_chain_with_an_image_member_fails_before_sending() {
    // Given: the worker pool's second member is an image model (left in place by a
    // normal load, which only warns).
    let (mut model, requests) = fixture_with(vec![None, None], &[], &[]);
    register(
        &mut model,
        "profile-1",
        "model-1",
        ModelKind::ImageGeneration,
    );
    // When: the worker sends, or a run is admitted, through that chain.
    let reply = send(&model, event_bus::RequestPurpose::Agent, None).await;
    let admitted = model
        .admit(
            &invocation(event_bus::RequestPurpose::Agent, None),
            Role::Worker,
        )
        .await;
    // Then: the whole chain is refused with a typed error and no member received a prompt.
    let expected = mismatch("chain `worker` route `worker`", "profile-1", "model-1");
    assert_eq!(reply, Err(expected.clone()));
    assert_eq!(admitted, Err(expected));
    assert!(requests.iter().all(|sent| sent.lock().unwrap().is_empty()));
    assert!(
        model
            .selected_model(Role::Worker, None)
            .starts_with("unresolved:")
    );

    // When: the same model is registered as a chat model instead.
    register(&mut model, "profile-1", "model-1", ModelKind::Chat);
    // Then: the chain is usable again.
    assert_eq!(
        send(&model, event_bus::RequestPurpose::Agent, None).await,
        Ok(response())
    );
}

#[tokio::test]
async fn a_summary_chain_with_an_image_member_fails_before_sending() {
    // Given: the worker uses route-0 only, and the compactor chain uses route-1, whose
    // only member is an image model.
    let (mut model, requests) = fixture_with(vec![None, None], &[None, None], &[]);
    model.agents.worker.base.models = vec![config::ChainEntryConfig::route("route-0")];
    model.agents.roles.compactor.models = vec![config::ChainEntryConfig::route("route-1")];
    register(
        &mut model,
        "profile-1",
        "model-1",
        ModelKind::ImageGeneration,
    );
    // When: the loop summarizes, then sends an ordinary request.
    let summary = send(&model, event_bus::RequestPurpose::Compaction, None).await;
    let reply = send(&model, event_bus::RequestPurpose::Agent, None).await;
    // Then: only the summary fails, and nothing reached the image model.
    assert_eq!(
        summary,
        Err(mismatch(
            "chain `compactor` route `route-1`",
            "profile-1",
            "model-1"
        ))
    );
    assert_eq!(reply, Ok(response()));
    assert!(requests[1].lock().unwrap().is_empty());
    assert_eq!(requests[0].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_selected_image_model_is_refused_before_sending() {
    // Given: the user picks an image model of a configured profile.
    let (mut model, requests) = fixture_with(vec![None, None], &[], &[]);
    register(
        &mut model,
        "profile-1",
        "model-1",
        ModelKind::ImageGeneration,
    );
    // When
    let reply = send(
        &model,
        event_bus::RequestPurpose::Agent,
        Some(ModelPreference {
            profile: "profile-1".into(),
            model: Some("model-1".into()),
            reasoning_effort: None,
        }),
    )
    .await;
    // Then
    assert_eq!(
        reply,
        Err(mismatch("the selected model", "profile-1", "model-1"))
    );
    assert!(requests.iter().all(|sent| sent.lock().unwrap().is_empty()));
}

#[test]
fn model_pickers_never_offer_image_defaults() {
    for explicit in [false, true] {
        let (mut model, _) = fixture_with(vec![None], &[], &[]);
        let id = if explicit { "model-0" } else { "gpt-image-1" };
        model
            .providers
            .get_mut("profile-0")
            .unwrap()
            .profile
            .default_model = id.into();
        if explicit {
            register(&mut model, "profile-0", id, ModelKind::ImageGeneration);
        }
        let profiles = model.available_profiles();
        assert_eq!(
            profiles[0].default_model, None,
            "image defaults must not re-enter the conversation picker"
        );
    }
}

#[test]
fn model_pickers_never_offer_image_models() {
    // Given: a profile that also lists an image model, recognized by its name.
    let (mut model, _) = fixture_with(vec![None], &[], &[]);
    model
        .providers
        .get_mut("profile-0")
        .unwrap()
        .profile
        .models
        .push("gpt-image-1".into());
    // When
    let profiles = model.available_profiles();
    // Then: only the chat model can be picked for a conversation.
    assert_eq!(profiles[0].models, ["model-0"]);
}
