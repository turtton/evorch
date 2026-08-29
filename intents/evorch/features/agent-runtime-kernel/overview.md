# Feature: Agent Runtime Kernel

[features 一覧](../) / [orchestration](../orchestration/overview.md) / [architecture](../../technology/architecture.md)

## 概要

Harness の中心は **Headless Agent Kernel** とする。GUI はその上に乗るだけで、Agent Runtime と UI は独立させる。

```text
Agent Kernel
├ Runtime
├ Orchestration
├ Context
├ Provider Routing
├ Tools
├ Sandbox
├ Storage
├ Diagnostics
└ Event Bus
```

各 AgentRun は独立 context を持ち、Tokio task として動作する。

## 要件

- AgentRun 構造: id / role / category / skills / route / context / policy を持つ
- Runtime 内部は event-driven とする（Started / MessageDelta / ReasoningDelta / ToolStarted / ToolCompleted / Delegated / BackgroundTaskStarted / BackgroundTaskCompleted / Usage / CacheStats / ProviderFallback / Completed / Failed）
- GUI は Event Stream を購読する。UI と runtime が密結合しない
- background agent を一級機能とする（delegate_background / send_message / wait / cancel）
- Session より下に Task 境界を持ち、1 conversation = 1 task に固定しない長寿命 workspace とする

## v0.1 role 実行 runtime の実装確定（2026-08-30）

`crates/agents/`（Role / RoleCapabilities / NetworkAccess・ADR 0002 capability 行列を `Role::capabilities()` にコード化）と `crates/runtime/`（AgentRun の 5 相状態遷移 Pending/Running/Waiting/Done/Error を EventBus へ event-sourced で emit）がコード確定（PR #16、issue #7）。要点:

- **capability 強制**: runtime は `RoleCapabilities` のみを消費し `Role` にマッチしない。Librarian / Oracle 追加は Role 定義 + capability 表の追加だけ（v0.2）
- **independent context**: `AgentContext` は run タスク専有で、複数 AgentRun が同時並行動作
- **background agent**: `delegate_background` / `send_message` / `wait` / `cancel` を `BackgroundTaskStarted` / `Completed` / `Cancelled` イベントで観測（GUI 非依存）
- **routing 委譲境界**: `AgentModel` trait が role → model routing の境界。v01-routing-profiles（実装中）が本 trait を実装する
- **orchestrator meta 操作**: 委譲系操作は ToolUse dispatch として runtime 内で処理

## 受け入れ基準

- AgentRun を Tokio task として起動・停止でき、各 run が独立 context を持つこと
- event stream が外部から購読でき、GUI 無しに runtime の挙動を観測できること
- background agent の起動・完了・キャンセルが event として観測できること

## Related decisions

- [ADR 0001: 固定 workflow を採用しない](../../decisions/0001-no-fixed-workflow.md)
- [ADR 0005: Headless Agent Kernel と GUI の分離](../../decisions/0005-headless-kernel-and-gui-separation.md)
- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)

## Open questions

- AgentRun の最大同時起動数の既定値
- Task boundary を自動検出するか、明示的なコマンドのみにするか
