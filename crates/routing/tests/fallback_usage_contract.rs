//! provider/routing 境界での usage exactly-once 契約を検証するハーネス。
//!
//! `Router::next_fallback` は候補の選択のみを行い usage を一切発行しない。
//! usage は勝利した attempt の provider client がちょうど 1 回だけ発行する。
//! coordinator (リトライ/フォールバックのループ) 自身はイベントバスに触れないため、
//! バス上に流れる usage / provider イベントは provider client の発行のみで構成される。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{Event, EventBus, EventKind, EventReceiver, ProviderEvent, UsageEvent};
use futures_util::StreamExt;
use model::{
    Availability, CatalogCapabilities, CatalogEntry, CatalogSource, LogicalModelId, ModelCatalog,
    ProviderType,
};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient,
    ProviderError, Role, StreamEvent,
};
use routing::{FailureKind, ProviderProfile, Router, SessionAffinity};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SESSION: &str = "session-usage-contract";
const LOGICAL: &str = "chat";

/// 非ストリーミング成功応答 (usage: prompt 11 / completion 7 / cached 3)。
const SEND_SUCCESS_BODY: &str = r#"{
  "id": "chatcmpl_usage_contract",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Hello."
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 11,
    "completion_tokens": 7,
    "total_tokens": 18,
    "prompt_tokens_details": {
      "cached_tokens": 3
    }
  }
}"#;

/// ストリーミング成功応答 (delta 1 frame + usage frame + [DONE])。
const STREAM_SUCCESS_BODY: &str = r#"data: {"id":"chatcmpl_usage_stream","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_usage_stream","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":3}}}

data: [DONE]
"#;

fn profile(name: &str, default_model: &str, base_url: String) -> ProviderProfile {
    let profile_config = config::ProviderProfileConfig {
        provider_type: config::ProviderTypeConfig::OpenAiCompatible,
        api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
        base_url,
        credential: config::CredentialRefConfig::Env {
            var: "API_KEY".to_string(),
        },
        models: vec![default_model.to_string()],
        default_model: default_model.to_string(),
    };
    ProviderProfile::try_from((name, &profile_config)).expect("有効な設定は変換できる")
}

fn candidate(profile: &str) -> config::RouteCandidateConfig {
    config::RouteCandidateConfig {
        profile: profile.to_string(),
        model: None,
    }
}

fn catalog_entry(model_id: &str) -> CatalogEntry {
    CatalogEntry {
        model_id: model_id.to_string(),
        provider: ProviderType::OpenAiCompatible,
        context_window: 64_000,
        max_output_tokens: 8_000,
        capabilities: CatalogCapabilities {
            tool_calling: true,
            reasoning: false,
            prompt_cache: false,
        },
        price: None,
        availability: Availability::Available,
        source: CatalogSource::Builtin,
        attributes_confirmed: true,
    }
}

fn catalog(model_ids: &[&str]) -> ModelCatalog {
    let mut catalog = ModelCatalog::builtin();
    for model_id in model_ids {
        catalog.merge_models_dev(vec![catalog_entry(model_id)]);
    }
    catalog
}

fn router(profiles: Vec<ProviderProfile>, candidates: Vec<config::RouteCandidateConfig>) -> Router {
    let routing = config::RoutingConfig {
        routes: [(LOGICAL.to_string(), candidates)].into_iter().collect(),
    };
    let model_ids: Vec<&str> = profiles.iter().map(|p| p.default_model.as_str()).collect();
    let catalog = catalog(&model_ids);
    Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる")
}

fn compat_client(uri: String, label: &str, bus: Option<Arc<EventBus>>) -> OpenAiCompatibleClient {
    OpenAiCompatibleClient::new(uri, label, Duration::from_secs(1), bus)
        .expect("互換clientを構築できる")
}

fn chat_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        observation: None,
    }
}

/// 50ms アイドルでバスからイベントを収集する (providers 側ヘルパと同じ意味論)。
async fn drain_events(rx: &mut EventReceiver) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Ok(event)) => events.push(event),
            Ok(Err(err)) => panic!("イベントの受信に失敗しました: {err:?}"),
            Err(_elapsed) => return events,
        }
    }
}

fn usage_events(events: &[Event]) -> Vec<&UsageEvent> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect()
}

fn provider_events(events: &[Event]) -> Vec<&ProviderEvent> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(provider) => Some(provider),
            _ => None,
        })
        .collect()
}

/// 契約対象の coordinator: フォールバック付き送信ループ。
///
/// Router による候補選択のみを行い、イベントバスには一切触れない。
async fn run_send_with_fallback(
    router: &Router,
    affinity: &mut SessionAffinity,
    clients: &BTreeMap<String, OpenAiCompatibleClient>,
    auth: &ProviderAuth,
    logical: &str,
) -> Result<ChatResponse, ProviderError> {
    let logical = LogicalModelId::from(logical);
    let mut route = router
        .resolve(affinity, SESSION, &logical)
        .expect("解決できる");
    loop {
        let client = clients
            .get(&route.profile)
            .expect("プロファイルに client がある");
        match client.send(auth, &chat_request(&route.model_id)).await {
            Ok(response) => return Ok(response),
            Err(error) => match router.next_fallback(
                affinity,
                SESSION,
                &logical,
                &route,
                FailureKind::from(&error),
                None,
            ) {
                Some(next) => route = next,
                None => return Err(error),
            },
        }
    }
}

/// 契約対象の coordinator: フォールバック付きストリームループ。
///
/// stream の開始に失敗したら次候補へフォールバックし、開始できた勝者の
/// DeltaStream を完了まで収集する。イベントバスには一切触れない。
async fn run_stream_with_fallback(
    router: &Router,
    affinity: &mut SessionAffinity,
    clients: &BTreeMap<String, OpenAiCompatibleClient>,
    auth: &ProviderAuth,
    logical: &str,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let logical = LogicalModelId::from(logical);
    let mut route = router
        .resolve(affinity, SESSION, &logical)
        .expect("解決できる");
    loop {
        let client = clients
            .get(&route.profile)
            .expect("プロファイルに client がある");
        match client.stream(auth, &chat_request(&route.model_id)).await {
            Ok(mut stream) => {
                let mut events = Vec::new();
                while let Some(event) = stream.next().await {
                    events.push(event.expect("勝者 stream は途中で失敗しない"));
                }
                return Ok(events);
            }
            Err(error) => match router.next_fallback(
                affinity,
                SESSION,
                &logical,
                &route,
                FailureKind::from(&error),
                None,
            ) {
                Some(next) => route = next,
                None => return Err(error),
            },
        }
    }
}

// Given: primary は 500・secondary は成功応答を返す 2 候補ルート
// When: coordinator がフォールバック付きで送信する
// Then: usage は勝者 (secondary) のみで 1 回、provider イベントは
//       primary Started->Failed の次に secondary Started->Completed が並ぶ
//       (wiremock .expect(1) により敗者 1 回・再送なしも検証される)
#[tokio::test(flavor = "multi_thread")]
async fn fallback_send_winner_emits_exactly_one_usage() {
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&primary)
        .await;
    let secondary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(SEND_SUCCESS_BODY, "application/json"),
        )
        .expect(1)
        .mount(&secondary)
        .await;

    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe(); // どの attempt より前に購読する
    let router = router(
        vec![
            profile("primary", "model-a", primary.uri()),
            profile("secondary", "model-b", secondary.uri()),
        ],
        vec![candidate("primary"), candidate("secondary")],
    );
    let clients: BTreeMap<String, OpenAiCompatibleClient> = [
        (
            "primary".to_string(),
            compat_client(primary.uri(), "primary-compat", Some(bus.clone())),
        ),
        (
            "secondary".to_string(),
            compat_client(secondary.uri(), "secondary-compat", Some(bus.clone())),
        ),
    ]
    .into_iter()
    .collect();
    let auth = ProviderAuth::new("sk-test");
    let mut affinity = SessionAffinity::default();

    let response = run_send_with_fallback(&router, &mut affinity, &clients, &auth, LOGICAL)
        .await
        .expect("secondary へフォールバックして送信に成功する");

    assert_eq!(response.finish_reason, FinishReason::Stop);
    let events = drain_events(&mut receiver).await;
    let usages = usage_events(&events);
    assert_eq!(usages.len(), 1, "usage は exactly-once: {events:?}");
    match usages[0] {
        UsageEvent::Usage {
            provider,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
        } => {
            assert_eq!(provider, "secondary-compat");
            assert_eq!(model, "model-b");
            assert_eq!(
                (
                    *input_tokens,
                    *output_tokens,
                    *cache_read_tokens,
                    *cache_write_tokens
                ),
                (11, 7, 3, 0)
            );
        }
        UsageEvent::CacheStats { .. } => panic!("Usage イベントを期待しました"),
    }

    let attempts = provider_events(&events);
    assert_eq!(attempts.len(), 4, "attempt 観測は 4 件: {events:?}");
    assert!(matches!(
        attempts[0],
        ProviderEvent::RequestStarted { provider, streaming: false, .. } if provider == "primary-compat"
    ));
    assert!(matches!(
        attempts[1],
        ProviderEvent::RequestFailed { provider, .. } if provider == "primary-compat"
    ));
    assert!(matches!(
        attempts[2],
        ProviderEvent::RequestStarted { provider, streaming: false, .. } if provider == "secondary-compat"
    ));
    assert!(matches!(
        attempts[3],
        ProviderEvent::RequestCompleted { provider, .. } if provider == "secondary-compat"
    ));
}

// Given: 単一プロファイルで 1 回目のみ 500・以降は成功応答 (優先度で分離)
// When: 同一 client へ失敗後に再送する (coordinator の同プロファイルリトライ相当)
// Then: usage は勝った 2 回目の attempt のみで 1 回発行される
#[tokio::test(flavor = "multi_thread")]
async fn retry_same_profile_emits_usage_only_for_winning_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(SEND_SUCCESS_BODY, "application/json"),
        )
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let router = router(
        vec![profile("primary", "model-a", server.uri())],
        vec![candidate("primary")],
    );
    let clients: BTreeMap<String, OpenAiCompatibleClient> = [(
        "primary".to_string(),
        compat_client(server.uri(), "primary-compat", Some(bus.clone())),
    )]
    .into_iter()
    .collect();
    let auth = ProviderAuth::new("sk-test");
    let mut affinity = SessionAffinity::default();

    let logical = LogicalModelId::from(LOGICAL);
    let route = router
        .resolve(&mut affinity, SESSION, &logical)
        .expect("解決できる");
    let client = clients
        .get(&route.profile)
        .expect("プロファイルに client がある");

    // 1 回目を送信し、失敗したら同一 client へリトライする (next_fallback を介さない)
    let mut result = client.send(&auth, &chat_request(&route.model_id)).await;
    if result.is_err() {
        result = client.send(&auth, &chat_request(&route.model_id)).await;
    }
    let response = result.expect("リトライした送信は成功する");

    assert_eq!(response.finish_reason, FinishReason::Stop);
    let events = drain_events(&mut receiver).await;
    let usages = usage_events(&events);
    assert_eq!(usages.len(), 1, "usage は exactly-once: {events:?}");
    match usages[0] {
        UsageEvent::Usage {
            provider, model, ..
        } => {
            assert_eq!(provider, "primary-compat");
            assert_eq!(model, "model-a");
        }
        UsageEvent::CacheStats { .. } => panic!("Usage イベントを期待しました"),
    }
}

// Given: 両候補とも 500 を返す 2 プロファイル構成 (secondary が最終候補)
// When: coordinator がフォールバックを尽くして送信する
// Then: Err を返し、usage は 0 件、Started->Failed が 2 組並ぶ
#[tokio::test(flavor = "multi_thread")]
async fn exhausted_fallback_emits_zero_usage() {
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&primary)
        .await;
    let secondary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&secondary)
        .await;

    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let router = router(
        vec![
            profile("primary", "model-a", primary.uri()),
            profile("secondary", "model-b", secondary.uri()),
        ],
        vec![candidate("primary"), candidate("secondary")],
    );
    let clients: BTreeMap<String, OpenAiCompatibleClient> = [
        (
            "primary".to_string(),
            compat_client(primary.uri(), "primary-compat", Some(bus.clone())),
        ),
        (
            "secondary".to_string(),
            compat_client(secondary.uri(), "secondary-compat", Some(bus.clone())),
        ),
    ]
    .into_iter()
    .collect();
    let auth = ProviderAuth::new("sk-test");
    let mut affinity = SessionAffinity::default();

    let error = run_send_with_fallback(&router, &mut affinity, &clients, &auth, LOGICAL)
        .await
        .expect_err("全候補が枯渇して送信は失敗する");
    assert!(matches!(error, ProviderError::Http { status: 500, .. }));

    let events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&events).is_empty(),
        "失敗時に usage は発行されない: {events:?}"
    );
    let attempts = provider_events(&events);
    assert_eq!(attempts.len(), 4, "Started->Failed が 2 組: {events:?}");
    assert!(matches!(
        attempts[0],
        ProviderEvent::RequestStarted { provider, .. } if provider == "primary-compat"
    ));
    assert!(matches!(
        attempts[1],
        ProviderEvent::RequestFailed { provider, .. } if provider == "primary-compat"
    ));
    assert!(matches!(
        attempts[2],
        ProviderEvent::RequestStarted { provider, .. } if provider == "secondary-compat"
    ));
    assert!(matches!(
        attempts[3],
        ProviderEvent::RequestFailed { provider, .. } if provider == "secondary-compat"
    ));
}

// Given: primary は 500・secondary は SSE 成功応答を返す 2 候補ルート
// When: stream coordinator がフォールバック付きでストリームを収集する
// Then: usage は勝者 (secondary) のみで 1 回、最後の canonical イベントは Completed
#[tokio::test(flavor = "multi_thread")]
async fn fallback_stream_winner_emits_exactly_one_usage() {
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&primary)
        .await;
    let secondary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(STREAM_SUCCESS_BODY, "text/event-stream"),
        )
        .expect(1)
        .mount(&secondary)
        .await;

    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let router = router(
        vec![
            profile("primary", "model-a", primary.uri()),
            profile("secondary", "model-b", secondary.uri()),
        ],
        vec![candidate("primary"), candidate("secondary")],
    );
    let clients: BTreeMap<String, OpenAiCompatibleClient> = [
        (
            "primary".to_string(),
            compat_client(primary.uri(), "primary-compat", Some(bus.clone())),
        ),
        (
            "secondary".to_string(),
            compat_client(secondary.uri(), "secondary-compat", Some(bus.clone())),
        ),
    ]
    .into_iter()
    .collect();
    let auth = ProviderAuth::new("sk-test");
    let mut affinity = SessionAffinity::default();

    let events = run_stream_with_fallback(&router, &mut affinity, &clients, &auth, LOGICAL)
        .await
        .expect("secondary へフォールバックしてストリームに成功する");

    assert!(matches!(
        events.last(),
        Some(StreamEvent::Completed { response }) if response.finish_reason == FinishReason::Stop
    ));
    let drained = drain_events(&mut receiver).await;
    let usages = usage_events(&drained);
    assert_eq!(usages.len(), 1, "usage は exactly-once: {drained:?}");
    match usages[0] {
        UsageEvent::Usage {
            provider, model, ..
        } => {
            assert_eq!(provider, "secondary-compat");
            assert_eq!(model, "model-b");
        }
        UsageEvent::CacheStats { .. } => panic!("Usage イベントを期待しました"),
    }
}
