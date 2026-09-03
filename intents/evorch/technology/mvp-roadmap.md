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

## v0.2 — 実装ループ内製化と GUI 再構成

grill `grill-v02-loop-foundation`（2026-09-02、11/11 accepted、`intents/evorch/interviews/grill-v02-loop-foundation.json`）で確定。`subagent-internalization`（2026-08-30）の messaging / workspace 計画を内包しつつ、loop 完結に必要な全層を v0.2 に確定する。

```text
Loop 基盤（packet 9 本）:
  v02-agent-messaging          reply / completion channel、transcript 永続、steering / wake
  v02-workspace-isolation      runtime 所有 worktree（evorch/task/<run-id>）、sandbox 内 git writable、project 許可ディレクトリ一体化
  v02-prompt-assembly          category→論理モデル config 結線、モデル別ベース最適化、preset / override 2層、
                               Orchestrator intent gate、provider/model fallback 区別（omo bug 非再現）
  v02-skill-loader             agentskills 仕様準拠（SKILL.md + bundled resources）、遅延ロード
  v02-project-rules            AGENTS.md ネスト closest wins、scoped rules、tool 実行後 synthetic 注入
  v02-context-compaction       75% 自動 + 手動、DCP 型 agent tool（cache 配慮の閾値調整）
  v02-provider-codex-subscription  ChatGPT Plus/Pro 経由 codex subscription（v0.3 から前倒し）
  v02-gui-workbench-restructure    t3code 基準レイアウト（左: project/thread、右: tabbed surfaces）、
                               Agents 一覧+drill-down+複数 pane 同時ライブ、最小 diff tab
  v02-orchestrator-loop        goal 固定、finish gate（composite gate + Reviewer 承認）、idle 駆動 continuation、
                               review 往復、停滞検知、人間 merge 承認のみ

既存 v0.2 seed（grill web-tools-v02 確定分）:
  NetworkGuard / web_search / web_fetch / OTel metrics exporter / OTel span exporter
```

**v0.2 進捗（2026-09-03、issue #55）**: OTel metrics exporter の slice ①（写像層＋OTLP metrics exporter）が landed。`crates/event-bus` に `event_bus::otel` module（写像層は常時 compile、SDK 接続は opt-in feature `otel-exporter`、既定 off）を追加し、`gen_ai.*` 標準属性＋`evorch.*` 拡張への写像・属性 whitelist 8 key の cardinality guard・golden fixture 検証・InMemory＋実 OTLP HTTP の二重 reader による最小 E2E を実装（詳細は ADR 0023 最終版）。`CacheStats`（hit/miss 系）は v0.3 以降送りを維持。残る v0.2 gap は slice ②（span exporter）のみとなった。

**成功基準**: evorch orchestrator が goal+contract 投入から worker 起動・実装・PR 作成・review 往復を経て人間 merge 承認まで GUI 起点で完走し（OpenCode / omo / herdr 非依存。GitHub / intent-cli 連携は shell tool 経由）、queue 済み v0.2 unit の後半 1-2 本を evorch 自身のループで消費できること（headless で再現可能）。

**v0.3 以降へ送り**: Librarian / Oracle role 追加、Role / Category separation の role 拡張面、Tree-sitter / LSP、ContentOrigin の web tools 外 generalization、provider affinity、cache metrics 単独項目（compaction / OTel で部分カバー。v0.2 slice ① で cache_read/cache_write の `gen_ai.token.type` 拡張値は出力済み、`CacheStats` の hit/miss 集計はこちらに残す）、github-copilot / anthropic-subscription provider（v0.3 計画維持）、diff / file tree 完全版。Planner / Multimodal の導入時期は別途決定（v0.3 以降の候補）。

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
