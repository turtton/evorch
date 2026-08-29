# ADR 0020: canonical message 正規化と OpenAI / Anthropic / OpenAI-compatible 変換を確定する

## Status

Accepted（2026-08-29、PR #13 で実装確定。issue #4 / v01-provider-client）

## Context

ADR 0004 は provider type と API protocol を分離したが、wire 形式と agent kernel の間で流れるメッセージの正規化形式（canonical message）が未確定だった。v0.1 の 3 provider（OpenAI / Anthropic / OpenAI-compatible）を同一 trait で扱うには canonical 形式と双方向変換が必須であり、mvp-roadmap の Open question「v0.1 provider 3 種で確定か」も同時に解決する。

## Decision

### canonical Message

`Message { role: Role, content: Vec<ContentBlock> }`。`Role { System, User, Assistant }`。`ContentBlock`（internally tagged `"type"`）: `Text` / `Reasoning` / `ToolUse { id, name, input }` / `ToolResult { tool_call_id, content, is_error }`。付随型: `ToolSpec { name, description, input_schema }` / `Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens }`（u64、provider 報告生値）/ `FinishReason { Stop, Length, ToolUse, ContentFilter, Other(String) }`（未知値を吸収）。

### ProviderClient trait

`#[async_trait] pub trait ProviderClient: Send + Sync`（dyn 互換、コンパイル時検証あり）:

- `capabilities(&self) -> ProviderCapabilities`
- `send(&self, auth: &ProviderAuth, request: &ChatRequest) -> Result<ChatResponse, ProviderError>`
- `stream(&self, auth, request) -> Result<DeltaStream, ProviderError>` — `DeltaStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>`。`StreamEvent { TextDelta | ReasoningDelta | ToolCallDelta | Completed }`（stream 内部で delta 結合し、最終 item が merge 済み応答を運ぶ）

auth は呼び出しごとに `ProviderAuth { api_key }` で注入 — クライアントは credential を保持しない（ADR 0008 整合）。base_url / timeout / EventBus はコンストラクタ設定。native async fn は dyn 非互換のため不採用。

### OpenAI ↔ Anthropic 変換の主要な非対称点（不可逆点は文書化）

- **system**: Anthropic はトップレベル `system` フィールド — canonical から hoisting（複数は `"\n\n"` 結合）。逆変換では `Role::System` メッセージとして戻る
- **tool result**: OpenAI は `role: "tool"` メッセージ、Anthropic は user turn 内 `tool_result` ブロック。`is_error` は OpenAI wire に無いため egress で失われ ingress では `false` で復元（不可逆）
- **reasoning**: Anthropic `thinking` ブロックは assistant のみ（user 側は `text` に変換）。OpenAI chat.completions に reasoning フィールドがないため egress で omit（不可逆）
- **max_tokens**: Anthropic Messages API では必須。canonical が `None` の場合 `DEFAULT_MAX_TOKENS = 4096` にフォールバック
- **SSE**: OpenAI は `data:` のみ（`[DONE]` 終端、`stream_options.include_usage` で最終 usage chunk）。Anthropic は named `event:`（`message_start` / `content_block_*` / `message_delta` / `message_stop` / `ping` / `error`）、usage が `message_start`（input/cache）と `message_delta`（output）に分散するため interpreter がマージ
- **usage cache**: OpenAI `prompt_tokens_details.cached_tokens` → `cache_read_tokens`（cache-write 指標は無いため `cache_write_tokens = 0` 恒常）。Anthropic `cache_read_input_tokens` / `cache_creation_input_tokens` → `cache_read_tokens` / `cache_write_tokens`

### usage イベント emit 経路

`UsageEmitter { bus: Option<Arc<EventBus>>, provider }` が発行を集約（`None` なら no-op）。send と stream の完了時に **ちょうど 1 回だけ** `UsageEvent::Usage` を emit。下流は `event_bus::UsageAggregator`（1 分バケット downsample）→ `UsageSink` → storage（ADR 0018）がそのまま繋がる。

### error taxonomy

`ProviderError`（thiserror 2）: `RateLimited { retry_after: Option<Duration> }`（429 + Retry-After parse）/ `Http { status, body }`（429 以外の 4xx/5xx）/ `Timeout`（send は全体 timeout、stream は connect/read timeout — 全体 timeout を掛けると長時間ストリームが殺されるため非対称）/ `InvalidSse` / `InvalidJson` / `Request`。付助 `status() -> Option<u16>`。Anthropic in-stream `error` イベントは `Http { status: 400, ... }` に写像（文書化済みフォールバック）。

### v0.1 provider 3 種の確定

OpenAI / Anthropic / OpenAI-compatible はコードとして確定。OpenAI-compatible は chat.completions wire を共用する独立クライアント（base_url / provider ラベルをコンストラクタ指定）。subscription 系（openai-codex / github-copilot / anthropic-subscription）は実装せず v0.3 で re-evaluation（再評価ノート §1）。

## Consequences

- mvp-roadmap の「v0.1 provider 3 種で確定か」Open question は解決（確定）
- routing / fallback / session affinity（v01-routing-profiles）は本 trait の上に実装する。credential 保存・設定・sandbox（v01-sandbox-approval）は auth 注入設計の外側として分離済み
- 検証は ADR 0015 第1層（wiremock + recorded fixture、実 API 不使用）。fixture は公式 API リファレンスベースの手書き
- 将来の新 provider 追加は canonical Message への ingress/egress 変換を実装するだけでよい — trait 不変
