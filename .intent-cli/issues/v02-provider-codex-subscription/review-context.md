# v02-provider-codex-subscription Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- codex subscription client が `ProviderClient` trait に実際に準拠し、auth が `ProviderAuth` の per-call 注入のままか。client 状態に access token / refresh token を保持する非互換な設計を持ち込んでいないか（既存契約: `crates/providers/src/client.rs` の credential 非保持）
- OAuth device flow / PKCE の実装が library-level API に留まり、認証起動 UI / 認証状態の GUI wiring を本 slice に持ち込んでいないか
- credential 保存が keyring-first / 0600 fallback の契約通りか。access token / refresh token を config ファイルへ書き込む経路・平文保存経路を作っていないか（ADR 0008 / ADR 0014、既存 strict field rejection の維持）
- access token / refresh token / account id が worker sandbox / bwrap 内子プロセス env に露出しない unit test があるか（ADR 0008 credential 分離）
- originator ヘッダーに自アプリ名を明示しているか。codex CLI を偽装するような identity 転用をしていないか（provider-routing overview 確定事項）
- ChatGPT-Account-Id が access token の JWT から導出されているか。ユーザー入力に依存しない機械導出か
- Codex backend body 制約（store / stream / max_output_tokens）の追随テストが存在し、backend 応答形式の変化を検知できるか
- attempt 観測（RequestStarted / FirstTokenObserved / RequestCompleted / RequestFailed）と usage exactly-once 契約が既存 3 client と同一配線か。新 client だけ usage を二重発行・欠落させる経路がないか
- API protocol の扱い（openai-codex-responses variant 追加か OpenAiResponses 流用か）の決定根拠が実装内で記録されているか。`openai`（API key 経由）type との分離が壊れていないか（ADR 0004）
- github-copilot / anthropic-subscription provider、subscription auth 状態の catalog availability 動的フィルタ、provider health / cooldown 高度化を v0.2 に持ち込んでいないか（grill Q12 および provider-routing overview の v0.3 / v0.4 項目）

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が credential 非露出・per-call auth 注入・usage exactly-once といった security / observation boundary の意味的接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/provider-routing/overview.md`: codex subscription provider の実装確定（codex_cli_simplified_flow OAuth の採用、endpoint / ヘッダー契約と backend body 制約の追随テスト結果、keyring-first / 0600 fallback の credential 保存先確定、API key 経由 openai type との分離維持）
- feasibility 検証の結果: ChatGPT backend endpoint 契約（device flow token endpoint の実 URL / 応答形式）の確認結果と未解決項目の明示。grill Q12 の v0.2 前倒し決定と github-copilot / anthropic-subscription の v0.3 維持の反映

記録が未実施の場合は、provider-routing overview の「サブスクリプション系 provider の実装方針」と実装の drift が残るため知識 writeback 不足として review 所見に残す。
