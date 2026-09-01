## Goal

ChatGPT Plus / Pro サブスクリプション経由の codex subscription provider を providers 層に実装する。認証は公式 codex_cli_simplified_flow OAuth（browser PKCE + device code 両対応）で token を取得・refresh し、credential は既存 security 基盤（keyring-first / 非搭載環境は 0600 fallback、config 平文は拒否済み）に載せる。client は既存 `ProviderClient` trait（send / stream / capabilities、auth は per-call 注入）の第 4 実装として、`chatgpt.com/backend-api/codex/responses` へ originator ヘッダー（自アプリ名明示）と ChatGPT-Account-Id ヘッダー（JWT 由来）を付けて送信する。attempt 観測と usage exactly-once の契約は既存 3 実装と同一配線を維持する。

## Why This Slice Exists Now

grill `grill-v02-loop-foundation`（11/11 accepted）Q12 で、v0.2 完了定義のドッグフーディング検証（evorch 自身の v0.2 unit を evorch のループで消費）を実運用モデルで行うために openai-codex subscription provider のみ v0.2 へ前倒しすることが確定した（github-copilot / anthropic-subscription は v0.3 維持）。provider-routing overview の「サブスクリプション系 provider の実装方針（2026-08 再評価済み）」節に OAuth 方式と endpoint 契約は確定済みだが、コード側は type 宣言だけで client 実装が存在しない「宣言だけの type」状態である。本 slice が無いと v0.2 の成功基準（実モデルでのループ完走）が API key 従量課金にしか接続できない。

## Current Observed State

- `crates/providers/` の client 実装は OpenAI / Anthropic / OpenAI-compatible の 3 種のみ。すべて API key 認証であり、subscription 系の client は存在しない。client の生成経路は `crates/providers/tests/` のみで production factory wiring は未整備
- `ProviderClient` trait（`crates/providers/src/client.rs:17-41`）は capabilities / send / stream の 3 メソッドで、auth は `ProviderAuth` をリクエストごとに引数注入し client 状態として保持しない契約（doc コメント明記）。dyn 互換のコンパイル時検証テストあり
- `ProviderTypeConfig::OpenAiCodex`（`crates/config/src/types/provider.rs:22-24`、serde 識別子 "openai-codex"）と `model::ProviderType::OpenAiCodex`（`crates/model/src/types.rs:20-22`）、routing 側 mapping（`crates/routing/src/profile.rs:55`）は既存だが、対応する client 実装がなく設定しても実行できない
- `ApiProtocolConfig` は anthropic-messages / openai-responses / openai-completions の 3 variant のみ（`crates/config/src/types/provider.rs:41`）。provider-routing overview の API Protocol 分離リストにある openai-codex-responses は未実装
- `CredentialRefConfig::{Keyring{service, account}, Env{var}}`（`crates/config/src/types/provider.rs:60`）は厳格ミラー Deserialize で未知フィールドを拒否。keyring 3（sync-secret-service / vendored）は workspace 依存既存で、`crates/sandbox/src/keychain.rs` に keyring::Entry 利用の既存実装がある
- config ロード経路は v0.1.1 strict field rejection により平文 credential（api_key / token / secret 等）を Keyring/Env 参照への remediation 付きで拒否する（ADR 0014 の load-time 強制）
- attempt 観測 5 variant（RequestStarted / FirstTokenObserved / RequestCompleted / RequestFailed / FallbackTriggered、request ID 相関）と usage exactly-once 契約（成功 attempt で 1 件、失敗で 0 件）は v0.1.1 で wiremock 契約テスト付き landed（`crates/providers/tests/observation_contract.rs` ほか）。新 client も同一配線が前提
- OAuth device flow / PKCE 用の補助クレート（oauth2 / rand / sha2 / base64 等）は workspace 未導入

## Accepted Baseline You May Assume

- ADR 0004: Provider Type / Profile / Logical Model / API Protocol の分離。codex（ChatGPT サブスクリプション）は `openai`（API key 経由）とは別 type として実装する
- ADR 0008: credential を agent プロセス・子プロセス・環境変数へ渡さない。access token / refresh token / account id は main process のみで消費する
- ADR 0014: config への平文 credential 書き込みは不可能（load-time 強制済み）。credential は Keyring/Env 参照のみ
- provider-routing overview「サブスクリプション系 provider の実装方針（2026-08 再評価済み）」: openai-codex は OpenCode / pi 方式の公式 codex_cli_simplified_flow OAuth（browser PKCE + device code 両対応）、originator ヘッダーに自アプリ名を明示、endpoint は chatgpt.com/backend-api/codex/responses、JWT 由来の ChatGPT-Account-Id ヘッダー必須、Codex backend の body 制約変化（store / stream / max_output_tokens）に追随テストが必須
- grill `grill-v02-loop-foundation.json` Q12: v0.2 前倒しは openai-codex のみ。github-copilot / anthropic-subscription は v0.3 維持
- v0.1.1 観測契約（request ID 相関の attempt 観測、usage exactly-once、TTFT 契約）は不変。新 client は既存契約テストと同型で pass する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/providers/`, `crates/config/`, `crates/model/`, `crates/routing/`

Target part: ChatGPT サブスクリプション（ChatGPT Plus / Pro）経由の codex subscription provider — OAuth device flow / PKCE 認証、keyring-first credential 保存（非搭載環境は 0600 fallback）、ProviderClient trait 実装（Codex responses 系 endpoint、originator / ChatGPT-Account-Id ヘッダー）

## In Scope

- `ProviderClient` trait 実装の codex subscription client（既存 3 実装のパターン踏襲、dyn 互換維持、`new` / `with_profile` 構成）
- OAuth device flow / PKCE 認証モジュール（device code 発行 → user_code / verification URL 返却 → polling → token 取得、refresh、期限切れ検知と自動 refresh）の library-level API 実装
- credential 保存: keyring-first（service / account は `CredentialRefConfig::Keyring` と整合）、keyring 非搭載環境は 0600 permission ファイル fallback。config 平文書き込み経路は作らない
- API protocol の確定: openai-codex-responses variant 追加か OpenAiResponses 流用かを Codex backend 実契約確認に基づき決定し、config / model / routing mapping に反映
- wire 契約: originator（自アプリ名明示）/ ChatGPT-Account-Id（JWT 由来）ヘッダー付与、Codex backend body 制約（store / stream / max_output_tokens）の追随テスト
- `AttemptObserver` / `UsageEmitter` の既存契約と同一配線
- `ProviderProfile`（provider_type: openai-codex）から client を構築する factory seam
- PKCE 用依存クレート（oauth2 / rand / sha2 / base64 等）の選定 feasibility 確認

## Out Of Scope

- github-copilot / anthropic-subscription provider — v0.3 維持（grill Q12）
- production `AgentModel` 実装（Router → ProviderClient 実行経路）への統合 — v02-prompt-assembly の scope
- 認証 dialog 等 GUI wiring — GUI workbench 側の後続 slice
- subscription auth 状態による model catalog availability 動的フィルタ — provider-routing overview の v0.3 項目
- OpenCode / pi-mono / codex CLI のコード取り込み — 参照調査のみ、直拷貝はしない
- 多アカウント切替 / organization 切替 UI
- provider health / cooldown 高度化 — v0.4

## Standalone Child Issue Contract

`turtton/evorch` で、ChatGPT Plus / Pro サブスクリプション経由の codex subscription provider を `ProviderClient` trait（capabilities / send / stream、auth は `ProviderAuth` per-call 注入、credential 非保持）の実装として `crates/providers/` に追加する。認証は公式 codex_cli_simplified_flow OAuth（browser PKCE + device code 両対応）の library-level API とし、device flow 全経路・refresh・期限切れ自動 refresh を wiremock 契約テストで検証する。credential は keyring-first（非搭載環境は 0600 permission ファイル fallback）で保存し、config 平文書き込み経路を作らず、access token / refresh token / account id が worker sandbox / bwrap 内子プロセス env に露出しない unit test を提供する。client は `chatgpt.com/backend-api/codex/responses` へ originator ヘッダー（自アプリ名明示）と JWT 由来 ChatGPT-Account-Id ヘッダーを付け、body 制約（store / stream / max_output_tokens）の追随テストを持つ。attempt 観測と usage exactly-once は既存 3 client と同一配線。`provider_type: "openai-codex"` の `ProviderProfile` から client を構築できる factory seam を提供し、API protocol の扱い（openai-codex-responses 追加か流用か）を実契約確認に基づき確定する。github-copilot / anthropic-subscription、production `AgentModel` 統合、認証 GUI、catalog availability 動的フィルタは実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- `provider_type: "openai-codex"` の ProviderProfile から Codex subscription client が構築でき（factory seam）、ProviderClient trait（send / stream / capabilities）として既存 3 実装と同型に動作する
- OAuth device flow / PKCE（codex_cli_simplified_flow 相当）による token 取得〜 refresh が wiremock 契約テストで検証済み（device code 発行 → polling → token 取得、refresh 経路、期限切れ検知と自動 refresh、polling 中断 / timeout の扱い）
- PKCE の code_verifier 生成と S256 challenge 計算が unit test で検証済み（依存クレート選定の feasibility 結果を含む）
- access token / refresh token は config に平文で書けず（既存 strict field rejection を維持）、keyring-first で保存される（keyring 非搭載環境では 0600 permission のファイル fallback）。unit test で検証済み
- access token / refresh token / account id が worker sandbox / bwrap 内子プロセス env に露出しないことを検証する unit test がある（ADR 0008 credential 分離）
- Codex backend request に originator ヘッダー（自アプリ名を明示）と ChatGPT-Account-Id ヘッダー（JWT 由来）が付与されることを wiremock 契約テストで検証済み
- chatgpt.com/backend-api/codex/responses の body 制約（store / stream / max_output_tokens 等）への追随テストがあり、backend 応答形式の変化を検知できる
- attempt 観測（RequestStarted / FirstTokenObserved / RequestCompleted / RequestFailed）と usage exactly-once 契約（成功 attempt で 1 件、失敗 attempt で 0 件）が既存 observation / usage 契約テストと同型で pass する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass

## Verification

- wiremock 契約テスト: device flow 全経路（device code 発行 → polling → token 取得）、refresh 経路、期限切れ検知と自動 refresh、polling 中断 / timeout
- PKCE unit test（code_verifier 生成、S256 challenge 計算、依存選定 feasibility 結果の記録）
- credential 保存 unit test（keyring-first / 0600 fallback、config 平文書き込み経路の不在）
- credential 非露出 unit test（worker sandbox / bwrap 内子プロセス env への露出なし、ADR 0008）
- wire 契約テスト（originator / ChatGPT-Account-Id ヘッダー、body 制約追随テスト）
- 既存契約の回帰: observation_contract（attempt 観測 + TTFT）、usage exactly-once、既存 3 client wire 契約テスト
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/provider-routing/overview.md（サブスクリプション系 provider の実装方針、2026-08 再評価済み）
- intents/evorch/decisions/0004-provider-routing-separation.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0014-config-architecture.md
- intents/evorch/interviews/grill-v02-loop-foundation.json
- 後続接続: `v02-prompt-assembly`（production AgentModel 実装 / Router → ProviderClient 実行経路）

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/provider-routing/overview.md` primary（「サブスクリプション系 provider の実装方針」節へ確定実装を反映）。supporting: ADR 0004 / 0008 / 0014、interviews/grill-v02-loop-foundation.json
- ADR candidate: none（OAuth 方式と endpoint 契約は 2026-08 再評価で確定済み。credential 保存は ADR 0008 / 0014 の既存決定の適用）
- Diagram candidate: none
- Docs update: none（role-facing surface の追加なし。Guide Reachability の宣言を参照）
- Closeout writeback expected: yes。codex provider の実装確定・endpoint / ヘッダー契約の追随テスト結果・credential 保存先確定・feasibility 未解決項目を provider-routing overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

本 slice が追加するのは provider client・認証・credential 保存の infrastructure であり、role が直接参照する surface（tool / meta tool / GUI surface）は増やさない。role からは論理モデルとしてのみ見えるため、`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
