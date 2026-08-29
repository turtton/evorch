# ADR 0006: Harness 自身の診断と自己改善

## Status

Accepted

## Context

Harness 自身の不具合を外部に気づくまで放置すると、継続的に品質が低下する。dogfooding による自己改善 loop を作り、runtime fault を自動収集・Issue 化したい。

## Decision

- 全 component は `DiagnosticBus` に診断イベントを送信する。
- Session / Task 終了時に Quick Diagnostic Agent が diagnostic bundle を分類する。
- Harness bug と判定されたら GitHub Issue 化する。
- Panic 等で session-end hook が実行できない場合は `~/.harness/crash-spool/` 等へ durable に保存し、次回起動時に処理する。
- Agent は introspection API（harness.inspect_session / inspect_agents / inspect_cache / inspect_provider / inspect_ui / spawn_test_instance / capture_ui / replay_interaction / report_bug）を利用できる。
- UI 自己改善は worktree → source modification → build → test harness instance → semantic inspection → screenshot / interaction replay で検証する。

## Consequences

- 診断情報の sanitized reproduction を作る必要がある。
- 自動 Issue 化の抑制条件（multi-fire 防止）を設ける必要がある。
- Self-improvement agent の権限範囲を慎重に決める必要がある。

## Related

- [features/diagnostics-self-improvement](../features/diagnostics-self-improvement/overview.md)
- [features/gui-workbench](../features/gui-workbench/overview.md)
- [features/agent-runtime-kernel](../features/agent-runtime-kernel/overview.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
