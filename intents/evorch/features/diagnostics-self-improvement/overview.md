# Feature: Diagnostics & Self-improvement（診断と自己改善）

[features 一覧](../) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md) / [gui-workbench](../gui-workbench/overview.md)

## 概要

Harness 自身の不具合を runtime が直接捕捉し、Issue 化し、dogfooding によって自分自身を改善可能にする。

## 要件

- **DiagnosticBus**: 全 component が Diagnostic（ProviderProtocolViolation / CacheRegression / ToolCrash / SandboxViolation / AgentDeadlock / UiError / CompactionFailure / SessionCorruption / UnexpectedModelSwitch 等）を送信する
- **Session 終了時の自動 Issue 化**: quick diagnostic agent が diagnostic bundle を分類（project problem / transient provider issue / probable harness bug）。harness bug と判断されたら version / OS / provider / model / event timeline / stacktrace / cache transition / tool call / sanitized reproduction をまとめて GitHub Issue 化
- **Crash spool**: panic 等で session-end hook が実行できない場合は `~/.harness/crash-spool/` 等へ durable に保存し、次回起動時に処理
- **Self-improvement introspection API**: harness.inspect_session / inspect_agents / inspect_cache / inspect_provider / inspect_ui / spawn_test_instance / capture_ui / replay_interaction / report_bug。不便を検出 → 改善案作成 → workspace config 変更または source 変更 → test instance → 検証 の自己改善 loop
- **UI 自己改善との連携**: Level 3 の framework implementation 変更は worktree → source modification → build → test harness instance → semantic inspection → screenshot / interaction replay で自己検証

## 受け入れ基準

- runtime fault が DiagnosticBus に流れ、診断バンドルが生成されること
- harness bug と分類された診断が GitHub Issue として作成されること
- session-end hook 非実行時も crash spool に記録され、次回起動時に処理されること

## Open questions

- 自動 Issue 化の抑制条件（誤検出の multi-fire 防止）
- self-improvement agent の権限範囲（config 変更のみか source 変更まで許可するか）
