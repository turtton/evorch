# v02-provider-codex-subscription Implementation Packet

## Goal

`crates/providers/` に ChatGPT Plus / Pro サブスクリプション経由の codex subscription provider を追加する。認証は公式 codex_cli_simplified_flow OAuth（browser PKCE + device code 両対応）で token を取得・refresh し、credential は既存 security 基盤（keyring-first、非搭載環境は 0600 permission のファイル fallback、config 平文は既存 strict field rejection が拒否）に載せる。client は既存 `ProviderClient` trait（`crates/providers/src/client.rs`、auth は `ProviderAuth` の per-call 注入、client 状態に credential を保持しない）の第 4 実装として、`chatgpt.com/backend-api/codex/responses` へ originator ヘッダー（自アプリ名明示）と ChatGPT-Account-Id ヘッダー（JWT 由来）を付けて送信する。attempt 観測（`AttemptObserver`）と usage exactly-once（`UsageEmitter`）の契約は既存 3 実装と同一配線。設定面では既存 `ProviderTypeConfig::OpenAiCodex`（"openai-codex"）を流用し、API protocol の扱い（openai-codex-responses variant 追加か OpenAiResponses 流用か）を Codex backend 実契約の確認結果に基づいて確定する。

## Why

grill `grill-v02-loop-foundation`（11/11 accepted、`intents/evorch/interviews/grill-v02-loop-foundation.json`）Q12 で、v0.2 完了定義のドッグフーディング検証（evorch 自身の v0.2 unit を evorch のループで消費）を実運用モデルで行うために openai-codex subscription provider のみ v0.2 へ前倒しすることが確定した（github-copilot / anthropic-subscription は v0.3 維持）。provider-routing overview の「サブスクリプション系 provider の実装方針（2026-08 再評価済み）」節には Codex OAuth の実装方式が確定済みだが、コード側は `ProviderTypeConfig::OpenAiCodex` と `model::ProviderType::OpenAiCodex`、routing mapping（`crates/routing/src/profile.rs:55`）が宣言されているだけで client 実装が存在しない「宣言だけの type」状態である。本 packet はこの解消と、実アカウントでのドッグフーディングに必要な credential 経路の確定を担う。

## Scope

- `ProviderClient` trait 実装の codex subscription client を `crates/providers/src/provider/` に追加する。既存 3 実装（OpenAI / Anthropic / OpenAI-compatible）のパターン（config 構造体、`new` / `with_profile`、wire 変換、timeout）を踏襲し、dyn 互換を維持する
- OAuth device flow / PKCE 認証モジュール: device code 発行 → user_code と verification URL の返却 → polling → token 取得、refresh 経路、期限切れ検知と自動 refresh を library-level API として実装する。認証起動の UI（dialog 等）は本 slice では抱えない
- credential 保存: keyring を優先し（service / account は `CredentialRefConfig::Keyring` と整合する形で profile から参照）、keyring 非搭載環境では 0600 permission のファイルに fallback する。access token / refresh token を config ファイルへ書き込む経路は作らない（ADR 0014 の load-time 強制を維持）
- API protocol の確定: `openai-codex-responses` を `ApiProtocolConfig` / `model::ApiProtocol` に新 variant 追加するか、`OpenAiResponses` を流用するかを Codex backend の実契約（chatgpt.com/backend-api/codex/responses の body 制約: store / stream / max_output_tokens 等）確認に基づいて決定し、`crates/config` / `crates/model` / `crates/routing` の mapping に反映する
- wire 契約: originator ヘッダーに自アプリ名を明示、ChatGPT-Account-Id（access token JWT 由来）を必須付与。backend 応答形式の変化を検知する追随テスト（body 制約 drift 検知）を実装する
- attempt 観測と usage 契約: `AttemptObserver` による RequestStarted / FirstTokenObserved / RequestCompleted / RequestFailed、`UsageEmitter` の exactly-once を既存 3 client と同一配線で維持する
- factory seam: `ProviderProfile`（provider_type: openai-codex）から client を構築する経路を提供する。production `AgentModel` 実装（Router → ProviderClient 実行経路）への統合は本 slice の scope 外（v02-prompt-assembly）

## Out of scope

- github-copilot / anthropic-subscription provider の実装 — grill Q12 で v0.3 維持と確定
- production `AgentModel` 実装（Router が解決した route を ProviderClient 実行に繋ぐ経路）— v02-prompt-assembly の scope
- 認証 dialog 等 GUI wiring — GUI workbench 側の後続 slice。本 slice は library API と wire 契約まで
- subscription auth 状態による model catalog availability 動的フィルタ — provider-routing overview 記載の v0.3 項目
- OpenCode / pi-mono / codex CLI のコード取り込み — 参照実装としての OAuth flow 調査のみ。直拷貝はしない
- 多アカウント切替 / organization 切替 UI
- provider health / cooldown 高度化 — v0.4（provider-routing overview 記載）

## Verification

- wiremock 契約テスト: device flow 全経路（device code 発行 → polling → token 取得）、refresh 経路、期限切れ検知と自動 refresh、polling 中断 / timeout の扱い
- PKCE unit test: code_verifier 生成の検証、S256 challenge 計算（依存クレート選定 feasibility 結果を packet 側に記録）
- credential 保存 unit test: keyring-first / 0600 fallback の選択、config 平文書き込み経路の不在（strict field rejection との整合）
- credential 非露出 unit test: access token / refresh token / account id が worker sandbox / bwrap 内子プロセス env に露出しないこと（ADR 0008）
- wire 契約テスト: originator / ChatGPT-Account-Id ヘッダー付与、Codex backend body 制約（store / stream / max_output_tokens）の追随テスト
- 既存契約の回帰: observation_contract（attempt 観測 + TTFT）、usage exactly-once 契約、既存 3 client の wire 契約テストが pass すること
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/provider-routing/overview.md` を primary とし、「サブスクリプション系 provider の実装方針」節に確定実装（OAuth device flow / PKCE、credential 保存、ヘッダー契約、body 制約追随テスト、v0.2 前倒し決定）を反映する。新規 intent は不要
- ADR candidate: decline — OAuth 方式と endpoint 契約は provider-routing overview の 2026-08 再評価で確定済み。credential 保存は ADR 0008 / 0014 の既存決定の適用であり新決定ではない
- Diagram candidate: decline — 認証 flow と client 構成は feature overview の記述で十分
- Docs update: decline — role-facing surface の追加はなし（provider は role から見える論理モデルの背後に置かれる。guide reachability の宣言は下記）
- Closeout learning: codex subscription provider の実装確定・endpoint / ヘッダー契約の追随テスト結果・credential 保存先の確定・feasibility 未解決項目の明示を provider-routing overview に記録する。`write_back_required: true`

- Guide reachability (G645): 本 slice が追加するのは provider client・認証・credential 保存の infrastructure であり、role が直接参照する surface（tool / meta tool / GUI surface）は増やさない。role からは論理モデルとしてのみ見えるため `no_role_facing_surface: true` を宣言する。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
