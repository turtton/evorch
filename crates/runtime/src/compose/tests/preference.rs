use super::*;
use crate::ModelPreference;

fn fixture() -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let (mut model, requests) = routed_model(Ok(response()), "model-a", None);
    let mut preferred = profile("model-b", &["model-b", "override-b"]);
    preferred.name = "profile-b".into();
    model.providers.insert(
        preferred.name.clone(),
        ComposedProvider {
            profile: preferred,
            client: Arc::new(StubClient {
                result: Ok(response()),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            auth: ProviderAuth::new("secret-b"),
        },
    );
    (model, requests)
}

async fn preferred(
    model: &RoutedModel,
    preference: Option<ModelPreference>,
) -> Result<ChatResponse, RuntimeError> {
    model
        .complete(
            &AgentInvocationContext {
                run_id: "run-preference".into(),
                model_preference: preference,
            },
            Role::Worker,
            &[],
            &[],
        )
        .await
}

#[tokio::test]
async fn complete_uses_model_preference_over_routing_rules() {
    // Given: only local is routed; profile-b has a distinct recording client.
    let (mut model, requests) = fixture();
    let preferred_requests = Arc::new(Mutex::new(Vec::new()));
    model.providers.get_mut("profile-b").unwrap().client = Arc::new(StubClient {
        result: Ok(response()),
        requests: preferred_requests.clone(),
    });
    // When
    let result = preferred(
        &model,
        Some(ModelPreference {
            profile: "profile-b".into(),
            model: Some("override-b".into()),
        }),
    )
    .await;
    // Then
    assert_eq!(result, Ok(response()));
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(preferred_requests.lock().unwrap()[0].model, "override-b");
}

#[tokio::test]
async fn complete_rejects_unknown_preferred_profile() {
    // Given
    let (model, requests) = fixture();
    // When
    let result = preferred(
        &model,
        Some(ModelPreference {
            profile: "missing".into(),
            model: None,
        }),
    )
    .await;
    // Then
    assert!(matches!(result, Err(RuntimeError::Model { reason }) if reason.contains("missing")));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn complete_falls_back_to_router_when_preference_none() {
    // Given
    let (model, requests) = fixture();
    // When
    assert_eq!(preferred(&model, None).await, Ok(response()));
    // Then
    assert_eq!(requests.lock().unwrap()[0].model, "model-a");
}

#[tokio::test]
async fn complete_rejects_unlisted_preferred_model() {
    // Given
    let (model, _) = fixture();
    // When
    let result = preferred(
        &model,
        Some(ModelPreference {
            profile: "profile-b".into(),
            model: Some("missing-model".into()),
        }),
    )
    .await;
    // Then
    assert!(
        matches!(result, Err(RuntimeError::Model { reason }) if reason.contains("missing-model") && reason.contains("profile-b"))
    );
}

#[tokio::test]
async fn preferred_failure_preserves_detail_without_fallback() {
    // Given
    let (mut model, requests) = fixture();
    model.providers.get_mut("profile-b").unwrap().client = Arc::new(StubClient {
        result: Err(ProviderError::Http {
            status: 401,
            body: "bad secret-b".into(),
        }),
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    // When
    let result = preferred(
        &model,
        Some(ModelPreference {
            profile: "profile-b".into(),
            model: None,
        }),
    )
    .await;
    // Then
    let Err(RuntimeError::Model { reason }) = result else {
        panic!("model error expected")
    };
    assert!(reason.contains("profile=profile-b model=model-b"));
    assert!(reason.contains("401") && reason.contains("bad ***"));
    assert!(!reason.contains("secret-b"));
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn routed_model_lists_available_profiles() {
    // Given
    let (model, _) = fixture();
    // When
    let summaries = model.available_profiles();
    // Then: stable lexical order, no credentials.
    assert_eq!(
        summaries,
        vec![
            ProfileSummary {
                name: "local".into(),
                provider_type: ProviderType::OpenAiCompatible,
                models: vec!["model-a".into(), "model-a".into()],
                default_model: Some("model-a".into())
            },
            ProfileSummary {
                name: "profile-b".into(),
                provider_type: ProviderType::OpenAiCompatible,
                models: vec!["model-b".into(), "override-b".into()],
                default_model: Some("model-b".into())
            },
        ]
    );
}

#[test]
fn switchable_profiles_follow_live_replacement() {
    // Given
    let (routed, _) = fixture();
    let expected = routed.available_profiles();
    let model = SwitchableModel::new(Arc::new(routed));
    assert_eq!(model.available_profiles(), expected);
    // When
    model.replace(Arc::new(UnconfiguredModel));
    // Then
    assert!(model.available_profiles().is_empty());
}
