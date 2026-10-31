//! Codex subscription backend の provider client を提供します。

mod compaction;
mod images;
mod web_search;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use futures_util::StreamExt;
use uuid::Uuid;

use super::oauth::DeviceAuthClient;
use super::session::{AuthorizedCodexToken, CodexSessionManager};
use super::tokens::CodexTokenStore;
use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::adapt_sse_stream;
use crate::http::{
    UsageEmitter, build_http_client, build_http_client_with_read_timeout, map_request_error,
    map_response_error,
};
use crate::message::{ChatRequest, ChatResponse, ProviderCapabilities};
use crate::models::{CODEX_CLIENT_VERSION, CodexModelCapabilities, CodexModelInfo};
use crate::observe::AttemptObserver;
use crate::stream::{DeltaStream, StreamEvent};
use crate::wire::codex::{CodexResponsesRequest, CodexStreamInterpreter};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(600);
const COMPACTION_READ_TIMEOUT: Duration = Duration::from_secs(300);
const PROVIDER_LABEL: &str = "openai-codex";
const PROTOCOL: &str = "openai-codex-responses";
const ORIGINATOR: &str = "codex_cli_rs";
/// Responses Lite の要求に付ける header (Codex CLI 0.166.0)。
const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// The run an observed request belongs to; an empty ID names no run.
fn run_id_of(observation: Option<&crate::message::ObservationContext>) -> Option<&str> {
    observation
        .map(|context| context.run_id.as_str())
        .filter(|run_id| !run_id.is_empty())
}

/// The identifier Codex calls the session/thread ID, derived from the run. Run IDs are
/// ULIDs, not UUIDs, so the UUID is a v5 over the run ID text: the same run always gets
/// the same value, in every request, resend and compaction, with no stored state.
fn run_derived_id(run_id: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, run_id.as_bytes()).to_string()
}

/// A request that only names the model, for scopes of capabilities without a chat request.
fn scope_probe(model: &str) -> ChatRequest {
    ChatRequest {
        responses_replay: None,
        model: model.to_owned(),
        messages: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

/// The resend after a recovery was rejected too: report a fixed error, never the
/// remote body, which would reach observation and logs.
fn reject_second_unauthorized(
    response: reqwest::Response,
    observer: &mut AttemptObserver,
) -> Result<reqwest::Response, ProviderError> {
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(response);
    }
    let error = ProviderError::Http {
        status: 401,
        body: "Codex rejected the refreshed credentials".into(),
    };
    observer.emit_failed(&error);
    Err(error)
}

/// Error categories deliberately exclude remote bodies, URLs, and credentials.
fn catalog_failure(error: &ProviderError) -> String {
    if let Some(status) = error.status() {
        return format!("http_{status}");
    }
    match error {
        ProviderError::Timeout => "timeout",
        ProviderError::Transport { .. } => "transport",
        ProviderError::InvalidJson { .. } => "invalid_json",
        _ => "other",
    }
    .into()
}

/// client の生存期間に 1 回だけ取得した catalog の結果。途中で要求の形状が変わって
/// prefix が壊れないよう、成功も失敗も固定する。失敗は能力不明 (従来の形状) として扱う。
struct CatalogOutcome {
    models: Result<Vec<CodexModelInfo>, ProviderError>,
}

impl CatalogOutcome {
    fn capabilities(&self, model: &str) -> Option<CodexModelCapabilities> {
        self.models
            .as_ref()
            .ok()?
            .iter()
            .find(|info| info.slug == model)
            .map(|info| info.capabilities.clone())
    }
}

#[derive(Clone, Copy)]
enum ResponseMode {
    Streaming,
    NonStreaming,
    Compaction,
}

/// base URL から responses endpoint を一意に解決します。正の形は
/// `https://chatgpt.com/backend-api/codex` で、二重 append による 404 を防ぐため
/// `/responses` 込み・裸ホスト等の指定も正規化します（pi の resolveCodexUrl 方式）。
fn resolve_endpoint(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else if normalized.ends_with("/backend-api") {
        format!("{normalized}/codex/responses")
    } else {
        format!("{normalized}/backend-api/codex/responses")
    }
}

/// The base URL a normalized responses endpoint hangs off, so the catalog and the
/// responses path agree for every accepted base URL shape (including `…/responses`).
fn catalog_base_url(endpoint: &str) -> String {
    endpoint
        .strip_suffix("/responses")
        .unwrap_or(endpoint)
        .to_owned()
}

/// Codex subscription backend client の設定。
#[derive(Clone)]
pub struct CodexConfig {
    /// Codex backend のベース URL。
    pub base_url: String,
    /// OAuth refresh endpoint のベース URL。
    pub auth_base_url: String,
    /// 通常の非ストリーミング生成と web search のリクエスト全体のタイムアウト。
    pub timeout: Duration,
    /// usage と attempt 観測イベントの発行先。
    pub event_bus: Option<Arc<EventBus>>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            auth_base_url: DEFAULT_AUTH_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            event_bus: None,
        }
    }
}

/// Codex subscription backend を canonical provider 契約へ接続します。
pub struct CodexClient {
    http_client: reqwest::Client,
    /// Compaction は通常生成より長い無応答期限と、本文受信までの全体期限を使う。
    compaction_http_client: reqwest::Client,
    endpoint: String,
    /// The base URL shared with `endpoint` (normalized by [`resolve_endpoint`]); catalog
    /// discovery appends `/models` to it.
    catalog_base_url: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
    profile: Option<String>,
    session: CodexSessionManager,
    /// クライアントの生存期間を通じて維持するセッション識別子。
    session_id: String,
    /// 生存期間中 1 回だけ取得する model catalog。要求の形状と `list_models` が共有する。
    catalog: tokio::sync::OnceCell<CatalogOutcome>,
}

impl CodexClient {
    /// 設定とセッションマネージャから client を構築します。
    ///
    /// # Errors
    /// HTTP client を構築できない場合に返します。
    pub fn new(config: CodexConfig, session: CodexSessionManager) -> Result<Self, ProviderError> {
        crate::responses_replay::validate_endpoint(&config.base_url)?;
        Ok(Self {
            http_client: build_http_client(None)?,
            compaction_http_client: build_http_client_with_read_timeout(
                Some(COMPACTION_TIMEOUT),
                COMPACTION_READ_TIMEOUT,
            )?,
            endpoint: resolve_endpoint(&config.base_url),
            catalog_base_url: catalog_base_url(&resolve_endpoint(&config.base_url)),
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: None,
            session,
            session_id: Uuid::new_v4().to_string(),
            catalog: tokio::sync::OnceCell::new(),
        })
    }

    /// 設定と token store から client を構築します。
    ///
    /// # Errors
    /// HTTP client を構築できない場合に返します。
    pub fn with_config(
        config: CodexConfig,
        store: Arc<dyn CodexTokenStore>,
    ) -> Result<Self, ProviderError> {
        crate::responses_replay::validate_endpoint(&config.base_url)?;
        let http_client = build_http_client(None)?;
        let session = CodexSessionManager::new(
            store,
            DeviceAuthClient::new(&config.auth_base_url, http_client.clone()),
        );
        Ok(Self {
            http_client,
            compaction_http_client: build_http_client_with_read_timeout(
                Some(COMPACTION_TIMEOUT),
                COMPACTION_READ_TIMEOUT,
            )?,
            endpoint: resolve_endpoint(&config.base_url),
            catalog_base_url: catalog_base_url(&resolve_endpoint(&config.base_url)),
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: None,
            session,
            session_id: Uuid::new_v4().to_string(),
            catalog: tokio::sync::OnceCell::new(),
        })
    }

    /// Associates telemetry with the configured profile without changing the wire request.
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    fn observer(&self, request: &ChatRequest, streaming: bool) -> AttemptObserver {
        AttemptObserver::new(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            self.profile.clone(),
            PROTOCOL,
            request.model.clone(),
            streaming,
            request.observation.clone(),
        )
        .with_service_tier(crate::wire::responses::codex_wire_service_tier(
            request.service_tier,
            self.catalog
                .get()
                .and_then(|outcome| outcome.capabilities(&request.model))
                .as_ref(),
        ))
    }

    async fn execute(
        &self,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<DeltaStream, ProviderError> {
        let model_name = request.model.rsplit('/').next().unwrap_or_default();
        if ["kimi", "moonshot"].iter().any(|prefix| {
            model_name
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        }) {
            return Err(ProviderError::Request(
                "Kimi full thinking requires an openai-compatible or kimi-subscription provider using openai-completions, not Codex reasoning summaries".into(),
            ));
        }
        let token = self.session.current().await?;
        let scope = self.responses_scope(request, &token);
        let wire_request = self.scoped_wire_request(request, &scope).await?;
        let mut observer = self
            .observer(request, streaming)
            .with_reasoning_effort(wire_request.reasoning_effort())
            .with_cache_observation(&wire_request);
        let response = self
            .send_response(
                request,
                &wire_request,
                &token,
                if streaming {
                    ResponseMode::Streaming
                } else {
                    ResponseMode::NonStreaming
                },
                &mut observer,
            )
            .await?;
        // Pass the shared interpreter itself: a forwarding wrapper silently drops
        // defaulted hooks such as take_message/finalize_message.
        Ok(adapt_sse_stream(
            response.bytes_stream(),
            CodexStreamInterpreter::with_origin(scope),
            UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL),
            request.model.clone(),
            observer,
        ))
    }

    fn responses_scope(
        &self,
        request: &ChatRequest,
        token: &AuthorizedCodexToken,
    ) -> crate::ResolvedResponsesScope {
        crate::ResolvedResponsesScope::resolve(
            request,
            crate::ResponsesDialect::Codex,
            self.profile.as_deref(),
            &self.endpoint,
            crate::ResponsesAccount::Codex {
                identity: token.chatgpt_account_id.clone(),
            },
        )
    }

    /// The wire body of one request. The model's catalog capabilities decide its shape
    /// (Responses Lite, reasoning, verbosity, service tier); the catalog is fetched once
    /// per client so the shape never changes under an established prompt-cache prefix.
    async fn scoped_wire_request(
        &self,
        request: &ChatRequest,
        scope: &crate::ResolvedResponsesScope,
    ) -> Result<CodexResponsesRequest, ProviderError> {
        // A mismatched scope is refused before the catalog is read.
        crate::wire::responses::check_scope(request, crate::ResponsesDialect::Codex, scope)?;
        let codex_model = self.ensure_catalog().await?.capabilities(&request.model);
        crate::wire::responses::to_wire_request_with_scope(
            request,
            &crate::wire::responses::ResponsesRequestOptions {
                dialect: crate::ResponsesDialect::Codex,
                // Codex's default (session-derived) cache key: one stable affinity key
                // per run, so every request of the run shares a cache namespace.
                prompt_cache_key: run_id_of(request.observation.as_ref()).map(run_derived_id),
                codex_model,
            },
            scope,
        )
    }

    /// Fetches the catalog on first use and keeps the outcome for the client's lifetime,
    /// failure included (the requests then keep the legacy shape). Only a missing session
    /// leaves it unfetched, since that is not an answer from the catalog. The flag tells
    /// whether this call performed the fetch. A failed fetch always emits one diagnostic;
    /// a successful one only when `announce_success` (catalog discovery asked for it).
    async fn catalog_outcome(
        &self,
        announce_success: bool,
    ) -> Result<(&CatalogOutcome, bool), ProviderError> {
        let fetched = std::sync::atomic::AtomicBool::new(false);
        let outcome = self
            .catalog
            .get_or_try_init(|| async {
                fetched.store(true, std::sync::atomic::Ordering::Relaxed);
                let token = self.session.current().await?;
                Ok(self.fetch_catalog(&token, announce_success).await)
            })
            .await?;
        Ok((outcome, fetched.into_inner()))
    }

    async fn ensure_catalog(&self) -> Result<&CatalogOutcome, ProviderError> {
        self.catalog_outcome(false)
            .await
            .map(|(outcome, _)| outcome)
    }

    async fn fetch_catalog(
        &self,
        token: &AuthorizedCodexToken,
        announce_success: bool,
    ) -> CatalogOutcome {
        let started = std::time::Instant::now();
        let models = crate::list_codex_models(
            &self.catalog_base_url,
            &ProviderAuth::new(token.access_token.clone()),
            &token.chatgpt_account_id,
        )
        .await;
        let outcome = CatalogOutcome { models };
        if announce_success || outcome.models.is_err() {
            self.emit_catalog_diagnostic(&outcome, false, started.elapsed());
        }
        outcome
    }

    fn emit_catalog_diagnostic(
        &self,
        outcome: &CatalogOutcome,
        cache_hit: bool,
        elapsed: Duration,
    ) {
        let Some(bus) = &self.event_bus else {
            return;
        };
        let failure = outcome.models.as_ref().err().map(catalog_failure);
        let count = outcome.models.as_ref().ok().map(Vec::len);
        bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
            source: "providers::codex::catalog".into(),
            severity: if failure.is_some() {
                event_bus::DiagnosticSeverity::Warning
            } else {
                event_bus::DiagnosticSeverity::Info
            },
            code: event_bus::event::diagnostic_codes::PROVIDER_CATALOG_FETCH.into(),
            detail: format!(
                "profile={} client_version={CODEX_CLIENT_VERSION} catalog_cache_hit={cache_hit} catalog_result={} model_count={} elapsed_ms={}",
                self.profile.as_deref().unwrap_or("unknown"),
                failure.as_deref().unwrap_or("ok"),
                count.map_or_else(|| "unknown".into(), |n| n.to_string()),
                elapsed.as_millis(),
            ),
            run_id: None,
            thread_id: None,
            call_id: None,
        }));
    }

    /// 通常生成と公式 compaction の HTTP エラー処理。
    ///
    /// 401 は 1 回だけ token を回復して同一 body で再送する。回復で
    /// ChatGPT account が変わった場合は、別 account の scope で送らず
    /// [`ProviderError::ResponsesScopeChanged`] を返す。
    async fn send_response(
        &self,
        request: &ChatRequest,
        wire_request: &CodexResponsesRequest,
        token: &AuthorizedCodexToken,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .post_response_with_token(
                request,
                wire_request,
                wire_request.is_responses_lite(),
                token,
                mode,
                observer,
            )
            .await?;
        if !response.status().is_success() {
            let error = map_response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        Ok(response)
    }

    /// All Responses paths share the selected account, refresh manager and headers.
    /// Status interpretation belongs to the caller (hosted search sanitizes errors).
    async fn post_response(
        &self,
        scope_request: &ChatRequest,
        wire_request: &impl serde::Serialize,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let token = self.session.current().await?;
        self.post_response_with_token(scope_request, wire_request, false, &token, mode, observer)
            .await
    }

    /// Sends once; a 401 triggers one recovery and a resend of the same wire body.
    /// The recovered credentials must belong to the same account (`scope_request`
    /// names the scope reported otherwise). A second 401 becomes a fixed error that
    /// carries no remote text.
    async fn post_response_with_token(
        &self,
        scope_request: &ChatRequest,
        wire_request: &impl serde::Serialize,
        responses_lite: bool,
        token: &AuthorizedCodexToken,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let run_id = run_id_of(scope_request.observation.as_ref());
        let response = self
            .post_response_once(wire_request, responses_lite, token, run_id, mode, observer)
            .await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        drop(response);
        let fresh = self
            .recover_unauthorized(scope_request, token, observer)
            .await?;
        let response = self
            .post_response_once(wire_request, responses_lite, &fresh, run_id, mode, observer)
            .await?;
        reject_second_unauthorized(response, observer)
    }

    /// Closes the rejected attempt as an auth failure, recovers credentials, refuses
    /// an account change and opens the next attempt. Errors are returned without a
    /// second terminal event.
    async fn recover_unauthorized(
        &self,
        scope_request: &ChatRequest,
        rejected_token: &AuthorizedCodexToken,
        observer: &mut AttemptObserver,
    ) -> Result<AuthorizedCodexToken, ProviderError> {
        // The rejected body is never read: dedicated capabilities sanitize their
        // errors, and the observation must not carry what they hide.
        observer.emit_failed_auth(&ProviderError::Http {
            status: 401,
            body: "unauthorized; recovering credentials".into(),
        });
        let fresh = self
            .session
            .refresh_after_unauthorized(rejected_token)
            .await?;
        if fresh.chatgpt_account_id != rejected_token.chatgpt_account_id {
            return Err(ProviderError::ResponsesScopeChanged {
                actual: Box::new(self.responses_scope(scope_request, &fresh)),
            });
        }
        observer.restart_attempt();
        Ok(fresh)
    }

    async fn post_response_once(
        &self,
        wire_request: &impl serde::Serialize,
        responses_lite: bool,
        token: &AuthorizedCodexToken,
        run_id: Option<&str>,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let http_client = match mode {
            ResponseMode::Compaction => &self.compaction_http_client,
            ResponseMode::Streaming | ResponseMode::NonStreaming => &self.http_client,
        };
        let mut builder = self
            .authenticated_request_with_token(http_client, &self.endpoint, token, run_id)
            .await
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&wire_request);
        if responses_lite {
            builder = builder.header(RESPONSES_LITE_HEADER, "true");
        }
        if matches!(mode, ResponseMode::NonStreaming) {
            builder = builder.timeout(self.timeout);
        }
        let http_request = builder.build().map_err(map_request_error)?;
        observer.emit_started();
        let response = http_client
            .execute(http_request)
            .await
            .map_err(map_request_error)
            .inspect_err(|error| observer.emit_failed(error))?;
        Ok(response)
    }

    /// Responses replay must use the exact lease already checked against its scope.
    ///
    /// `session-id`, `thread-id` and `x-client-request-id` carry the run-derived ID when
    /// the request belongs to a run, so they never change within it. Without a run they
    /// fall back to the client's session ID and a fresh ID per HTTP request.
    async fn authenticated_request_with_token(
        &self,
        http: &reqwest::Client,
        endpoint: &str,
        token: &AuthorizedCodexToken,
        run_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let (session_id, thread_id) = match run_id {
            Some(run_id) => {
                let id = run_derived_id(run_id);
                (id.clone(), id)
            }
            None => (self.session_id.clone(), Uuid::new_v4().to_string()),
        };
        http.post(endpoint)
            .bearer_auth(&token.access_token)
            .header("chatgpt-account-id", &token.chatgpt_account_id)
            .header("originator", ORIGINATOR)
            .header("version", CODEX_CLIENT_VERSION)
            .header(
                reqwest::header::USER_AGENT,
                format!("codex_cli_rs/{CODEX_CLIENT_VERSION}"),
            )
            .header("session-id", session_id)
            .header("thread-id", &thread_id)
            .header("x-client-request-id", &thread_id)
    }
}

/// `ProviderAuth` は使用せず、セッションの token bundle から認証します。
#[async_trait]
impl ProviderClient for CodexClient {
    async fn resolve_responses_scope(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<Option<crate::ResolvedResponsesScope>, ProviderError> {
        let token = self.session.current().await?;
        Ok(Some(self.responses_scope(request, &token)))
    }
    /// Discovery goes through the request session so an idle profile refreshes its token,
    /// and uses the client version its requests send.
    async fn list_models(&self, _: &ProviderAuth) -> Result<Option<Vec<String>>, ProviderError> {
        let (outcome, fetched) = self.catalog_outcome(true).await?;
        if !fetched && outcome.models.is_ok() {
            self.emit_catalog_diagnostic(outcome, true, Duration::ZERO);
        }
        outcome
            .models
            .as_ref()
            .map(|models| Some(models.iter().map(|model| model.slug.clone()).collect()))
            .map_err(|error| match error.clone() {
                ProviderError::Http { status, .. } => ProviderError::Http {
                    status,
                    body: "catalog verification failed".into(),
                },
                ProviderError::Request(_) | ProviderError::Transport { .. } => {
                    ProviderError::Request("catalog verification failed (transport/auth)".into())
                }
                ProviderError::InvalidJson { .. } => ProviderError::InvalidJson {
                    detail: "catalog verification failed".into(),
                },
                other => other,
            })
    }

    fn hosted_web_search(&self) -> Option<&dyn crate::HostedWebSearch> {
        Some(self)
    }

    fn image_generation(&self) -> Option<&dyn crate::ImageGeneration> {
        Some(self)
    }

    fn compactor(&self) -> Option<&dyn crate::Compactor> {
        Some(self)
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true,
        }
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let mut stream = self.execute(request, false).await?;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Completed { response } = event? {
                return Ok(response);
            }
        }
        Err(ProviderError::Request(
            "codex response ended without completion".to_string(),
        ))
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.execute(request, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_endpoint;

    #[test]
    fn resolve_endpoint_normalizes_base_url_shapes() {
        let canonical = "https://chatgpt.com/backend-api/codex/responses";
        for (input, expected) in [
            ("https://chatgpt.com/backend-api/codex", canonical),
            ("https://chatgpt.com/backend-api/codex/", canonical),
            ("https://chatgpt.com/backend-api/codex/responses", canonical),
            ("https://chatgpt.com/backend-api", canonical),
            ("https://chatgpt.com", canonical),
        ] {
            assert_eq!(resolve_endpoint(input), expected, "input: {input}");
        }
    }
}
