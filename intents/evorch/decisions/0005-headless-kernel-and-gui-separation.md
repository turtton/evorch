# ADR 0005: Headless Agent Kernel と GUI の分離

## Status

Accepted

## Context

GUI を application architecture の中心にすると、GUI framework の変更が全体に影響する。Floem の先行評価の結果次第で egui + egui_dock へ切り替える可能性があるため、framework 非依存な中間層が必要。

## Decision

以下の層構造とする。

```text
Agent Kernel
    ↓
UI Event Bus
    ↓
Workspace Model
    ↓
GUI Renderer
```

- Agent Kernel は headless な runtime 中核とする。
- GUI は Event Stream を購読するのみ。
- Workspace Model / Layout は framework-independent data として保持する。
- 第一 GUI 候補は Floem（fallback: egui + egui_dock）。

## Consequences

- GUI framework を切り替えても Workspace Model / UI Event Bus は保てる。
- Agent から GUI を操作する Semantic UI API は Workspace Model 上で実装される。
- Floem prototype で mouse UX / dock / undock / multi-window / large transcripts を評価する。

## Related

- [features/agent-runtime-kernel](../features/agent-runtime-kernel/overview.md)
- [features/gui-workbench](../features/gui-workbench/overview.md)
- [features/diagnostics-self-improvement](../features/diagnostics-self-improvement/overview.md)
- [technology/architecture](../technology/architecture.md)
