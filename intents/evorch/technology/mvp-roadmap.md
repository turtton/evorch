# MVP Roadmap（v0.1 – v0.5）

[architecture](architecture.md) / [product overview](../product/overview.md)

最初から全機能を作らない。段階的に拡張する。

## v0.1 — 最小構成の動くもの

```text
Rust / Tokio / SQLite / egui + egui_dock prototype（ADR 0007）

Native Agent Runtime

Provider: OpenAI / Anthropic / OpenAI-compatible

Agents: Orchestrator / Explorer / Worker / Reviewer

Tools: read / edit / grep / shell / git diff

Security (ADR 0008 v0.1):
  sandbox + approval 二層分離
  credential 隔離（keychain 優先 / 0600 fallback）
  network egress 既定 deny
  制御マーカー エスケープ

Features:
  independent agent contexts
  background agent
  provider profile
  simple fallback
  event stream
  session persistence
  basic GUI panes
```

**成功基準**: Orchestrator が依頼を受け、Explorer/Worker/Reviewer を background 起動し、event stream が観測でき、GUI で複数 pane が表示され、session が SQLite に永続化される。

**v0.1.1 進捗（2026-08-30、PR #30）**: 製品 GUI（`evorch-gui`）が実 AgentRuntime へ wiring 済み。`EmptyAgentSource` は廃止され、runtime と EventPump が同一 `Arc<EventBus>` を共有する。`--demo` は外部 AI provider 不要の決定的 scripted session で、tasks pane に name/role/status/model の live 表示と Pending→Running→Done 遷移を確認できる（手順は `evorch-gui --help` に同梱）。残る v0.1 GUI 側の gap は文字内容レイアウトの automated 検証（headless screenshot 基盤が前提）と実 provider 配線時の routing 実装。

## v0.2 — 役割の深化と観測

```text
Librarian / Oracle
Role / Category separation
Tree-sitter / LSP
ContentOrigin 実装（ADR 0008）
web_search / web_fetch（tools-sandbox 側ー v0.2 web ツール確定節参照）
project trust（ロード制御、ADR 0008）
provider affinity
cache metrics
```

**成功基準**: v0.1 の4 role（Orchestrator / Explorer / Worker / Reviewer）に Librarian / Oracle が追加され、計6 role が capability boundary として分離動作し、Librarian が web_search / web_fetch（tools-sandbox 側ー v0.2 web ツール確定）で外部調査でき、cache hit ratio が計測され、sandbox policy が role ごとに適用される。Planner / Multimodal の導入時期は別途決定（v0.3 以降の候補）。

## v0.3 — プロバイダ拡張と cache 高度化

```text
openai-codex subscription provider（ChatGPT Plus/Pro 経由）
github-copilot provider（device code OAuth。AI Credits 課金）
anthropic-subscription provider（Claude Pro/Max 経由）
cache-aware wait
agent-triggered compaction
OpenAI official compaction
```

**成功基準**: subscription 系 provider が profile として利用でき、cache-aware wait が長時間 command 中に cache TTL を維持し、agent 判断での compact が動作する。

## v0.4 — メモリとルーティング高度化

```text
Memory
Task boundary
multi-task sessions
advanced routing
provider health / cooldown
```

**成功基準**: task boundary を跨いで memory が反映され、provider health に基づく routing/cooldown が動作する。

## v0.5 — 自己改善

```text
Diagnostics
automatic Issue creation
self-improvement agent
semantic UI introspection
test harness instance
```

**成功基準**: runtime fault が DiagnosticBus に流れ、harness bug が自動 Issue 化され、semantic UI API 経由で agent が UI を検査・改善できる。

## Open questions

- v0.1 の GUI は egui + egui_dock で基本 pane（agent / terminal / tasks）とする（ADR 0007 で確定）。Floem 評価用 prototype は必須ではなく任意の並行調査
- ~~v0.1 で用意する provider は OpenAI / Anthropic / OpenAI-compatible の3種で確定か~~ → 2026-08-29 確定（PR #13 で `ProviderClient` 3 実装としてコード化、ADR 0020）
- Planner / Multimodal role の導入 version
- 各 version のリリース基準（tag / ブランチ戦略）
