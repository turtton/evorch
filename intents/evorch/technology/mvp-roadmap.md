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

## v0.2 — 役割の深化と観測

```text
Librarian / Oracle
Role / Category separation
Tree-sitter / LSP
sandbox
provider affinity
cache metrics
```

**成功基準**: 8 role が capability boundary として分離動作し、cache hit ratio が計測され、sandbox policy が role ごとに適用される。

## v0.3 — プロバイダ拡張と cache 高度化

```text
OpenAI Codex subscription provider
GitHub Copilot provider
Claude subscription provider
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

- v0.1 の GUI は Floem prototype のみで許容するか（基本 pane のみ）
- v0.1 で用意する provider は OpenAI / Anthropic / OpenAI-compatible の3種で確定か
- 各 version のリリース基準（tag / ブランチ戦略）
