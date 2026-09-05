use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use config::{GenerationOverridesConfig, RoleBindingConfig, RouteCandidateConfig, RoutingConfig};
use model::{ApiProtocol, ModelCatalog, ProviderType};
use providers::{
    ChatRequest, ChatResponse, ContentBlock, DeltaStream, FinishReason, Message, ProviderAuth,
    ProviderCapabilities, ProviderClient, ProviderError, Role as MessageRole, Usage,
};
use routing::{ComposedProvider, ComposedProviders, CredentialRef, ProviderProfile, Router};

use super::*;

#[derive(Clone)]
struct StubClient {
    result: Result<ChatResponse, ProviderError>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl ProviderClient for StubClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        self.result.clone()
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        Err(ProviderError::Request("stream unsupported".to_string()))
    }
}

fn response() -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        },
        usage: Usage::default(),
        finish_reason: FinishReason::Stop,
    }
}

fn profile(default_model: &str, models: &[&str]) -> ProviderProfile {
    ProviderProfile {
        name: "local".to_string(),
        provider_type: ProviderType::OpenAiCompatible,
        api_protocol: ApiProtocol::OpenAiCompletions,
        base_url: "http://127.0.0.1:1/v1".to_string(),
        credential: CredentialRef::Env {
            var: "TEST_KEY".to_string(),
        },
        models: models.iter().map(ToString::to_string).collect(),
        default_model: default_model.to_string(),
    }
}

fn routed_model(
    result: Result<ChatResponse, ProviderError>,
    default_model: &str,
    route_model: Option<&str>,
) -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let profile = profile(
        default_model,
        &[default_model, route_model.unwrap_or(default_model)],
    );
    let mut catalog = ModelCatalog::builtin();
    catalog.merge_discovered(profile.models.clone());
    let router = Router::new(
        vec![profile.clone()],
        &RoutingConfig {
            routes: BTreeMap::from([(
                "worker".to_string(),
                vec![RouteCandidateConfig {
                    profile: "local".to_string(),
                    model: route_model.map(ToString::to_string),
                }],
            )]),
        },
        catalog,
    )
    // SAFE-EXPECT: fixture profile, route, and catalog are constructed together above.
    .expect("valid routed model fixture");
    let providers = BTreeMap::from([(
        "local".to_string(),
        ComposedProvider {
            profile,
            client: Arc::new(StubClient {
                result,
                requests: Arc::clone(&requests),
            }),
            auth: ProviderAuth::new("secret-never-rendered"),
        },
    )]);
    (
        RoutedModel::new(
            ComposedProviders { router, providers },
            config::AgentsConfig {
                worker: RoleBindingConfig {
                    generation: GenerationOverridesConfig {
                        temperature: Some(0.25),
                        max_tokens: Some(321),
                        ..GenerationOverridesConfig::default()
                    },
                    ..RoleBindingConfig::default()
                },
                ..config::AgentsConfig::default()
            },
        ),
        requests,
    )
}

async fn complete(model: &RoutedModel, run_id: &str) -> Result<ChatResponse, RuntimeError> {
    model
        .complete(
            &AgentInvocationContext {
                run_id: run_id.to_string(),
            },
            Role::Worker,
            &[Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "work".to_string(),
                }],
            }],
            &[],
        )
        .await
}

// Given: worker binding の生成設定と default model を持つ stub provider
// When: RoutedModel.complete を呼ぶ
// Then: route/model/history/settings/run observation が ChatRequest に写される
#[tokio::test]
async fn complete_builds_request_from_binding_and_route() {
    let (model, requests) = routed_model(Ok(response()), "local-model", None);

    let result = complete(&model, "run-7").await;

    assert_eq!(result, Ok(response()));
    let recorded = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].model, "local-model");
    assert_eq!(recorded[0].temperature, Some(0.25));
    assert_eq!(recorded[0].max_tokens, Some(321));
    assert_eq!(
        recorded[0]
            .observation
            .as_ref()
            .map(|value| value.run_id.as_str()),
        Some("run-7")
    );
}

// Given: secret を含む provider error を返す stub client
// When: RoutedModel.complete が失敗する
// Then: RuntimeError::Model は安全な固定理由だけを返す
#[tokio::test]
async fn complete_redacts_provider_error_detail() {
    let (model, _) = routed_model(
        Err(ProviderError::Request("secret-never-rendered".to_string())),
        "local-model",
        None,
    );

    let error = complete(&model, "run-8")
        .await
        .expect_err("provider failure");

    assert_eq!(
        error,
        RuntimeError::Model {
            reason: "provider request failed".to_string()
        }
    );
}

// Given: worker logical model が local profile の default model に解決される adapter
// When: selected_model を呼ぶ
// Then: profile/model_id 形式の identity を返す
#[test]
fn selected_model_formats_profile_and_model() {
    let (model, _) = routed_model(Ok(response()), "local-model", None);

    assert_eq!(model.selected_model(Role::Worker), "local/local-model");
}

// Given: route candidate の model override と別 default model を持つ profile
// When: 同じ run を二度、別 run を一度 complete する
// Then: 同一 run は profile pin 後の default model、別 run は未 pin の override を使う
#[tokio::test]
async fn complete_keeps_affinity_per_run_id() {
    let (model, requests) = routed_model(Ok(response()), "default-model", Some("route-model"));

    assert_eq!(complete(&model, "run-a").await, Ok(response()));
    assert_eq!(complete(&model, "run-a").await, Ok(response()));
    assert_eq!(complete(&model, "run-b").await, Ok(response()));

    let models = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|request| request.model.clone())
        .collect::<Vec<_>>();
    assert_eq!(models, ["route-model", "default-model", "route-model"]);
}
