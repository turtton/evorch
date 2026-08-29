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

## 受け入れ基準

- AgentRun を Tokio task として起動・停止でき、各 run が独立 context を持つこと
- event stream が外部から購読でき、GUI 無しに runtime の挙動を観測できること
- background agent の起動・完了・キャンセルが event として観測できること

## Open questions

- AgentRun の最大同時起動数の既定値
- Task boundary を自動検出するか、明示的なコマンドのみにするか
