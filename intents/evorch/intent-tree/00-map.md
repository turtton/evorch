# Intent Map

- Domain: `evorch`
- Target repo: `turtton/evorch`
- Source concept: [agent-harness-concept.md](../../../agent-harness-concept.md)（AI-Native Agent Harness 構想）
- Entrypoints: [mission](../identity/mission.md) / [product overview](../product/overview.md) / [mvp-roadmap](../technology/mvp-roadmap.md)

## identity/

- [mission.md](../identity/mission.md) — 使命・ビジョン・原則・用語集（Intent Gate / Execution Shape / Stable Prefix / Logical Model 等）

## product/

- [overview.md](../product/overview.md) — 製品概要、目標ユーザー、ユーザージャーニー、non-goals、最終イメージ（Agent Operating Environment）

## features/（8領域）

- [agent-runtime-kernel](../features/agent-runtime-kernel/overview.md) — Headless Agent Kernel、AgentRun、event-driven architecture、background agent、Task boundary
- [orchestration](../features/orchestration/overview.md) — Intent Gate、Execution Shape、Role capability boundary、delegation policy、Agent 5軸分解
- [provider-routing](../features/provider-routing/overview.md) — Provider Type/Profile 分離、Logical Model、API Protocol、fallback、session affinity、capability
- [context-engine](../features/context-engine/overview.md) — Stable Prefix、cache metrics、cache-aware wait、compaction、memory
- [gui-workbench](../features/gui-workbench/overview.md) — Floem workbench、workspace model 分離、semantic UI API、subagent 可視化、UI 自己改善
- [tools-sandbox](../features/tools-sandbox/overview.md) — tool trait、shell/PTY、code intelligence、sandbox policy、MCP
- [storage-memory](../features/storage-memory/overview.md) — SQLite event sourcing、entity 群、memory パイプライン、session/task 構造
- [diagnostics-self-improvement](../features/diagnostics-self-improvement/overview.md) — DiagnosticBus、自動 Issue 化、crash spool、introspection API、自己改善 loop

## technology/

- [architecture.md](../technology/architecture.md) — Agent Kernel 構成、crate 分割案、技術スタック
- [mvp-roadmap.md](../technology/mvp-roadmap.md) — v0.1–v0.5 の段階的ロードマップと成功基準

## 未配置カテゴリ

- operations/ / decisions/ / clarifications/ / packets/ / links/ — 実装・運用が進むにつれて追加する。decisions/ には ADR（`<NNNN>-<slug>.md`）を、packets/ には roadmap/backlog/waves を配置する。
