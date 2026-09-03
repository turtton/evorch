# ADR 0001: 固定 workflow を採用しない

## Status

Accepted

## Context

一般的な Coding Agent ツールは `Explore → Plan → Execute → Review → Fix` のような固定状態機械を中核にしている。しかし evorch のターゲットとなる依頼は調査・実装・バグ解析・コードレビュー・設計相談・ドキュメント作成・リポジトリ探索・リファクタリング・比較検証・単純な質問など多様であり、固定フローは必ずしも最適ではない。

## Decision

固定 workflow を採用せず、以下の流れを採用する。

```text
User Request
    ↓
Intent Gate
    ↓
Execution Shape
    ↓
Orchestrator / Direct Agent
    ↓
Dynamic Agent Topology
```

- Intent Gate は粗い `ExecutionShape`（Direct / Coordinated）だけを決める。
- Coordinated の場合、Orchestrator が required capabilities から動的に topology を構築する。
- entry ではローカルキーワードルール（omo `ulw` 型）で「明らかに単一作業」を判定し、該当すれば Worker 直接起動、該当しなければ Orchestrator 起動へ分岐する。デフォルトは Orchestrator 起動、明示 direct キーワード時のみ Worker 直接起動。fail-safe は Orchestrator 側。
- Direct として起動した run が「これは複数モジュールまたぐ / 依存調査が必要 / 並列化したい」と気づいた場合、専用 meta op で `EscalationMemo`（構造化スキーマ: `source_run_id`, `original_request`, `findings`, `files_touched: Vec<PathBuf>`, `blockers`, `workspace_state(dirty files/summary)`, `escalation_reason`, `suggested_next`）を記録して旧 run を terminal にし、新しい Orchestrator root run を起動する。workspace は引き継ぐ（旧 run terminal 保証後に排他的に譲渡）。

## Consequences

- 依頼の種類に応じた最適な Agent 構成を作れる。
- Orchestrator は capability boundary と delegation value を元に自己判断で subagent を起動する。
- タスク追跡や再現性の担保は event log / session persistence で補う。

## Related

- [features/orchestration](../features/orchestration/overview.md)
- [features/agent-runtime-kernel](../features/agent-runtime-kernel/overview.md)
- [product/overview](../product/overview.md)
