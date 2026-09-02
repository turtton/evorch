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

v0.2 で messaging op 再編と workspace 隔離を予定している。後述の「v0.2 計画」節を参照。

## v0.2 計画: メッセージング再編と workspace 隔離（grill subagent-internalization 確定、2026-08-30）

oh-my-pi（can1357/oh-my-pi、commit 51f0380 調査）の設計をベースに、evorch の event sourcing（ADR 0018）・親子ツリー topology・Linux-first sandbox（ADR 0021/0009）へ合わせて改修して取り込む。以下は v0.2 ターゲットの計画であり、現行 v0.1 実装との gap を明示する。宛先規則（親子限定ツリー addressing）と `can_delegate` の Role capability 開放の決定自体は [ADR 0022](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md) を参照。

### messaging op 再編（`crates/runtime/src/meta.rs`）

- `send`: fire-and-forget。相手の応答や完了を待たない
- `wait_reply`: timeout 付き reply waiter。返信が必要な場合のみ使用
- `inbox`: 未読メッセージの pull 参照

現行 `send_message` は送信後に相手 run の完了を待つ同期寄りの形であり、この 3 op 構成に再編する。宛先検証は delegation tree の親子関係で行う（ADR 0022）。

### 配送语义

- 相手が busy の場合、sender が親なら steering（実行中の turn に注入）、sender が親でなければ aside（step boundary まで保留）。親子ツリー規則と整合する注入 policy
- 相手が Waiting / idle なら wake
- parked（session 解放済み）なら DM で revive
- 完了通知と mid-run relay の channel を分離する: `AgentMessage` イベントを `BackgroundTaskCompleted` / finish 結果の経路から独立させる（`crates/event-bus` 拡張）

ADR 0018（SQLite event sourcing）上に置くため、oh-my-pi の cap 100 非 durable mailbox と異なり、メッセージの durable 化・監査・再送制御が構造的に可能。

### RunConfig.workspace_mode と git worktree backend（`crates/runtime/src/run.rs`）

- `workspace_mode = shared`: 親と同じ cwd で動作（現行相当）
- `workspace_mode = isolated`: runtime が委譲時に git worktree を作成し run の cwd として绑定。bwrap fs sandbox（ADR 0021/0009、Linux-first）と組み合わせ、worktree を rw、それ以外を policy 通りに mount する
- worktree の作成・破棄・merge は harness（evorch runtime）が所有する
- merge mode: `patch`（差分を .patch artifact として親へ返す）と `branch`（`evorch/task/<run-id>` branch に commit し親が merge/cherry-pick）を両サポート。branch が既定（herdr-opencode-loop の実運用が branch + PR フローのため整合）
- 並列 worker が同一 checkout を触る競合をツール側で防げるようになる

### parked 状態

AgentRunPhase に parked 相当の状態（または Done + revive 経路）を追加し、session を解放した agent が DM で revive できるようにする。

**スコープ修正（grill grill-v02-loop-foundation Q2、2026-09-02）**: 厳密な revive（会話・tool 状態のスナップショット復元を含む durable inbox）は v0.3 送りとする。v0.2 では AgentMessage（send / reply / steering）を Event Bus イベントとして transcript 永続化（message repo 既存）し、run crash 時は親が新規 run を起動して transcript から文脈を再構成する運用とする。durable 化・監査基盤（ADR 0018 event sourcing）の上に置く構造は維持。

**実装確定（issue #47、PR #48、2026-09-02）**: 上記 messaging op 再編が runtime/event-bus/storage に実装済み。envelope は `crates/event-bus/src/event.rs` の `AgentMessage`（message_id / sender_run_id / recipient_run_id / kind（send/reply/steering）/ content / reply_to 任意 / disposition）で、serialization は `EventKind::AgentMessage` として lifecycle event とは別種。配送 core は `crates/runtime/src/mailbox.rs`（fire-and-forget send、wait_reply の reply_to 相関+typed timeout、inbox は配送順 pull・既読は再返却しない）。親子限定 addressing（ADR 0022）は meta / runtime 両層で全入口強制、sibling・無関係 run・自己宛は fail-closed。steering（親→Running 宛に進行中 turn 注入）/ aside（非親→step boundary まで保留）/ wake（Waiting 宛を Running へ）は agent loop 内で実装済み。永続化は Event Bus → storage bridge → `agent_messages_by_session` 読取 API（順序 / sender / recipient / correlation の復元可）。crash 復旧は親が新規 run を起動して transcript から文脈を再構成する方式で実装・検証済み（厳密 revive は引き続き v0.3）。meta op 面は `meta/messaging.rs` へ再編（旧 `send_message` は fire-and-forget alias 化）。既存 delegate / wait / cancel / orchestration consumer と `orchestrator_demo` の回帰は無差異。

### loop 基盤の関連 packet 索引（grill grill-v02-loop-foundation、2026-09-02）

本 feature（kernel）の v0.2 計画は messaging / workspace / parked が対象。loop 完結に必要な残り層は別 packet が担う（詳細は `technology/mvp-roadmap.md` v0.2 節と各 packet）: `v02-prompt-assembly`（category routing + モデル別最適化 + preset/override 2層 + intent gate）、`v02-skill-loader`（agentskills 準拠）、`v02-project-rules`（AGENTS.md ネスト + tool 後注入）、`v02-context-compaction`（75% 自動 + 手動 + DCP 型）、`v02-orchestrator-loop`（goal 固定 + finish gate + continuation）。

### 参照

oh-my-pi（can1357/oh-my-pi）の参照は commit 51f0380 の調査に基づく。参照ファイル: `registry/agent-lifecycle.ts`（idle → parked → revive）、`registry/agent-tree.ts`、`irc/bus.ts`（mailbox + waiter + delivery receipt）、`session/irc-bridge.ts`（steer / aside）、`task/engine.ts`、`config/agents-config.ts`、`messaging.ts`、`projections/pipeline.ts`。

## 受け入れ基準

- AgentRun を Tokio task として起動・停止でき、各 run が独立 context を持つこと
- event stream が外部から購読でき、GUI 無しに runtime の挙動を観測できること
- background agent の起動・完了・キャンセルが event として観測できること

## Related decisions

- [ADR 0001: 固定 workflow を採用しない](../../decisions/0001-no-fixed-workflow.md)
- [ADR 0005: Headless Agent Kernel と GUI の分離](../../decisions/0005-headless-kernel-and-gui-separation.md)
- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)
- [ADR 0022: 親子限定ツリー addressing と can_delegate の Role capability 開放](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md)

## Open questions

- AgentRun の最大同時起動数の既定値
- Task boundary を自動検出するか、明示的なコマンドのみにするか
- 最大委譲深度の確定値（ADR 0022 では推奨 2–3。実装時に確定）
- parked run の状態保持方針（revive 可能な期限・event stream 上の扱い）
