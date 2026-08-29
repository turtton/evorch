# v01-event-stream Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

Particular focus for this slice.

- transport は in-process（tokio broadcast）のみ。分散 / 外部 transport（gRPC / WebSocket / OTLP executor 等）を実装して scope を広げないこと。将来方針は ADR 0017 の記述に留める。
- SQLite 永続化・downsampled 書き込みは v01-session-storage の担当。本 slice は ring buffer + in-memory 集計の「土台」とインターフェースまで。
- 受け入れ基準の内容に含まれない event 型（orchestration / context / diagnostics 由来等）まで schema に足して広げないこと。v0.1 で agent-runtime-kernel が列挙する event 群を中心にし、拡張は versioning 経由で行うこと。
- ADR 0017 と architecture.md の Open questions 更新（G461 writeback）が本 PR または明示的な follow-up packet として含まれることを確認する。

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`（ADR 0017 生成 + architecture.md transport Open question 更新）。ADR 0017 が本 PR で生成され、architecture.md の「Event Bus の transport 実装」が解決済みとして更新されたことを確認する。ADR 0017 はこの packet の contract（acceptance criteria 最後の項目）でもあるため省略は不可。