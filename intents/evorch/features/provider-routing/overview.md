# Feature: Provider Routing（プロバイダルーティング）

[features 一覧](../) / [context-engine](../context-engine/overview.md) / [architecture](../../technology/architecture.md)

## 概要

Provider は Agent Runtime と完全に分離する。Agent SDK を中心に据えず、Claude / OpenAI / ChatGPT Codex / GitHub Copilot 等を普通の Model Provider として扱う。pi と同様に Provider ≠ API Protocol とする。

## 要件

- **Provider Type / Profile 分離**: type（anthropic / anthropic-subscription / openai / openai-codex / github-copilot / openrouter / openai-compatible）と credential instance を同一視しない。同一 type の上に複数 Profile を作れる（claude-personal / claude-business / copilot-work 等）
- **Model / Provider 分離**: Logical Model を上位概念にし、route → provider profile → API protocol に解決する（例: claude-main → claude-business → claude-personal → openrouter）
- **API Protocol 分離**: anthropic-messages / openai-responses / openai-completions / openai-codex-responses / google-generative-ai / copilot-compatible。ProviderProfile が protocol を選択する
- **Provider Capability**: prompt_cache / reasoning / tool_calling / compaction / streaming / transport の capability を明示する
- **fallback**: 単純 round-robin ではなく、current provider profile → same model / another profile → alternative logical model の順
- **Session affinity**: prompt cache のため同一 task / session で profile に留まる。429 / 5xx / timeout / quota / auth で cooldown 管理。Retry-After を優先
- **provider health / cooldown 管理**（v0.4 で拡張）

## v0.1 provider 3 種の実装確定（2026-08-29）

OpenAI / Anthropic / OpenAI-compatible が `ProviderClient` 実装としてコード確定（PR #13、issue #4）。canonical message 正規化と wire 変換は [ADR 0020](../../decisions/0020-canonical-message-normalization.md)。usage イベントは `UsageEmitter` 経由で event stream へ emit され、`UsageAggregator`（ADR 0012）→ storage（ADR 0018）にそのまま繋がる。検証は wiremock 契約テスト（ADR 0015 第1層、実 API 不使用）。subscription 系は v0.3 で re-evaluation。

## v0.1 config / model / routing 層の実装確定（2026-08-30）

`crates/config/` + `crates/model/` + `crates/routing/` がコード確定（PR #17、issue #8）。要点:

- **config**: TOML マルチソース読み込み（CLI/環境変数 > project `./evorch.toml` > user `~/.config/evorch/config.toml` > builtin defaults）+ `config.d/*.toml` 辞書順 deep merge（後勝ち）+ version フィールドと migration 関数 + schemars JSON Schema 生成。v0.1 設定領域の typed struct（provider profiles / model routing / panel layout・keybind / diagnostics / permission preset / 計測）。GUI（v01-gui-panes）の panel layout・keybind は workspace-ui 内の最小設定型で先行実装されており、本 config 層への統合は後続 slice
- **model**: ModelCatalog の4供給源のうち v0.1 実装分 — builtin デフォルト（オフライン返却）+ models.dev 起動時 fetch（キャッシュ + builtin フォールバック）+ `/v1/models` 検出マージ（属性未確定フラグ付き）。subscription 系の auth 状態動的フィルタは v0.3。更新履歴は SQLite `catalog_updates` テーブル（migration V2、append-only）
- **routing**: TOML の複数 provider profile（credential は参照のみ・config 非書き込み）→ logical model → route → profile → 実モデル ID 解決。simple fallback（current profile → 同じ logical model の別 profile → 別 logical model。429 / 5xx / timeout / quota / auth で遷移）+ session affinity の基礎。health / cooldown 高度化は v0.4


## v0.1.1 config strict field rejection の実装確定（2026-08-30）

config ロード経路が fail-closed 化され、未知キーと平文 credential の黙殺が不可能になった（PR #24、issue #23）。要点:

- **strict walker**: `crates/config/src/strict.rs` が deep merge + version migration 後・型パース直前のマージ済み値を歩き、Config root と全 nested struct の許可フィールドを allowlist 検査。エラーは `providers.foo.api_key` / `routing.routes.fast[0].weight` のような dotted config path 付き `ConfigError::InvalidField`。builtin / user / project / drop-in / env / CLI override の全ソース層に一様に適用される
- **平文 credential の明示拒否**: `providers.<profile>` 直下と `credential` テーブル内で `api_key` / `api-key` / `token` / `secret` / `password` / `credential_value` 等を検出すると、Keyring/Env reference 形式への remediation 付きで拒否（ADR 0014 のロード経路強制）。キー照合は小文字化 + `-`→`_` 正規化。任意キーを許容する map は `providers` のプロファイル名・`routing.routes` の route 名・`panel.keybinds` のキーのみ
- **型側防衛**: 8 struct（Config / ProviderProfileConfig / RoutingConfig / RouteCandidateConfig / PanelConfig / DiagnosticsConfig / PermissionConfig / MetricsConfig）に `deny_unknown_fields`。`CredentialRefConfig` は内部タグ enum で serde 属性が使えないため、private ミラー構造体 + 手動 Deserialize で variant 単位の拒否を実現（wire format・Serialize・JsonSchema は不変）
- **留意**: strict.rs の allowlist 定数は struct 定義と手動同期（field 追加時は両方更新する）。`EVORCH_API_KEY` のような root 未知キー env による load が unknown-key エラーになるのは意図挙動

## サブスクリプション系 provider の実装方針（2026-08 再評価済み）

- **anthropic-subscription**: senpi（code-yeongyu/senpi）方式。正規 OAuth authorization-code + PKCE（`claude.ai/oauth/authorize` → `platform.claude.com/v1/oauth/token`、scope に `user:sessions:claude_code`）。access token を Messages API の apiKey として使用。Claude Code 風 tool 命名の模倣（Stealth mode）を実装。refresh は期限 5 分前に provider 単位 lock 下で実施。pi-mono も同経路を現役で保持。
- **openai-codex**: OpenCode / pi 方式。公式 codex_cli_simplified_flow OAuth（browser PKCE + device code 両対応）、`originator` ヘッダーに自アプリ名を明示。endpoint は `chatgpt.com/backend-api/codex/responses`、JWT 由来の `ChatGPT-Account-Id` ヘッダー必須。`openai`（API key 経由）とは別 type として実装。Codex backend の body 制約変化（store/stream/max_output_tokens）には追随テストが必須。
- **github-copilot**: device code OAuth（`api.githubcopilot.com/chat/completions`）。2026-06 から usage-based 課金（AI Credits 制）に移行済みで「定額無制限」前提はない旨をユーザー向け表示に反映。

## モデルカタログ（ADR 0013）

モデル情報は4供給源のハイブリッド: ①組み込みデフォルト（属性・価格）②起動時 fetch（models.dev 等、キャッシュ+オフラインフォールバック）③プロバイダ検出（openai-compatible の `/v1/models`、属性未確定フラグ付きマージ）④サブスクリプション系の auth 状態動的フィルタ。ModelCatalog は domain transform 対象（ADR 0010）。価格カタログはコスト計算（ADR 0012）と同一ソース。

## v0.1.1 provider 観測イベントと TTFT 契約の実装確定（2026-08-30、PR #32 / issue #31）

- `EventKind::Provider` 配下に attempt 観測 5 variant を追加（`RequestStarted` / `FirstTokenObserved` / `RequestCompleted` / `RequestFailed` / `FallbackTriggered`）＋失敗分類 `ProviderFailureKind`。全イベントは attempt ごと一意の `request_id`（`req-<プロセス起動ms>-<単調カウンタ>`）で相関。`SCHEMA_VERSION=1` 不変・追加のみ・legacy snapshot テストで後方互換を固定
- **発行境界**: `AttemptObserver`（crates/providers/src/observe.rs）を attempt ごとに生成し、wire request 構築成功後・HTTP 送信直前に `RequestStarted`。成功・失敗併せて終端はちょうど 1 回（flag + Drop backstop で consumer drop は `Other` として終端化）。OpenAI / Anthropic / OpenAI-compatible 3 クライアントに同一配線
- **TTFT 契約**: 開始=上記開始点、終了=最初の非空 text delta か tool-call delta の正常解釈。headers / usage-only / keepalive / 空 delta / reasoning-only は first token に数えない。streaming 成功で高々 1 回、非 streaming では発行しない
- **token accounting**: `RequestCompleted` の token counts は同一 attempt の `UsageEvent::Usage` の観測用複製。集計 canonical は UsageEvent のみ（二重計上禁止、wire 順序 `Started → Usage → Completed` で相関。UsageEvent に request ID は持たせない wire 不変制約）
- **FallbackTriggered**: `Router::next_fallback` の選択境界のみ発行（`with_event_bus` で接続）。候補枯渇（None 返却）では発行しない。順序・policy は不変で観測追加のみ

## 受け入れ基準

- provider type と profile を TOML で複数定義でき、logical model から解決できること
- fallback が「同じ model の別 profile → 別 logical model」の順で試行されること
- 同一 session で provider affinity が保たれ、失敗時のみ cooldown 付きで切り替わること
- config の未知キーが dotted config path 付きでロード時に拒否されること
- credential が config に平文で書けず、Keyring/Env 参照への remediation 付きで拒否されること（ADR 0014 の load-time 強制。PR #24）
- 全 provider client の request attempt が request ID 相関の開始/終端観測イベントとして bus に流れること、streaming は TTFT（上記契約通り）も高々 1 回流れること（PR #32）
- fallback 選択が FallbackTriggered として観測でき、失敗分類が型付き（ProviderFailureKind）であること（PR #32）

## Related decisions

- [ADR 0004: Provider Type / Profile / Logical Model / API Protocol の分離](../../decisions/0004-provider-routing-separation.md)
- [ADR 0003: Cache-first Context Engine](../../decisions/0003-cache-first-context-engine.md)
- [ADR 0020: canonical message 正規化と OpenAI / Anthropic / OpenAI-compatible 変換](../../decisions/0020-canonical-message-normalization.md)

## Open questions

- subscription 系 provider の認証フロー詳細（実装方式は [再評価ノート](../../technology/re-evaluation-2026-08.md) §1 で確定済み。Codex backend の body 制約変化への追随テスト方針）
- capability の未対応時の degrading 方針（cache 非対応 provider での扱い等）
