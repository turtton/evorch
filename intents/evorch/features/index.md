# Features 一覧

[Intent Map](../intent-tree/00-map.md) / [Product Overview](../product/overview.md)

evorch の主要 feature 領域。各 overview は要件・受け入れ基準・open questions を含む。

- [agent-runtime-kernel](agent-runtime-kernel/overview.md) — Headless Agent Kernel、AgentRun、event-driven architecture、background agent、Task boundary
- [orchestration](orchestration/overview.md) — Intent Gate、Execution Shape、Role capability boundary、delegation policy、Agent 5軸分解
- [provider-routing](provider-routing/overview.md) — Provider Type/Profile 分離、Logical Model、API Protocol、fallback、session affinity、capability
- [context-engine](context-engine/overview.md) — Stable Prefix、cache metrics、cache-aware wait、compaction、memory
- [gui-workbench](gui-workbench/overview.md) — egui + egui_dock workbench（ADR 0007）、workspace model 分離、semantic UI API、subagent 可視化、UI 自己改善
- [tools-sandbox](tools-sandbox/overview.md) — tool trait、shell/PTY、code intelligence、sandbox policy、MCP
- [storage-memory](storage-memory/overview.md) — SQLite event sourcing、entity 群、memory パイプライン、session/task 構造
- [diagnostics-self-improvement](diagnostics-self-improvement/overview.md) — DiagnosticBus、自動 Issue 化、crash spool、introspection API、自己改善 loop
