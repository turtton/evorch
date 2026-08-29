# v01-session-storage Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

Particular focus for this slice.

- schema は acceptance criteria の entity（sessions / tasks / messages / agent_runs / events / downsampled_metrics）に限定。memory や provider_health 等、後続/他 slice が扱う entity を先行追加して scope を広げないこと。
- raw 高頻度計測の直接永続化をしないこと（ADR 0012）。downsampled のみ、single-writer + バッチ flush 経路にすること。
- credential の永続化は控えること（ADR 0008）。API key をテーブル列や日志に含めないこと（review-context に該当なし）。
- ADR 0018 と storage-memory/overview.md の更新（G461 writeback）が本 PR または明示的な follow-up packet として含まれることを確認する。

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

`closeout_learning.write_back_required` は `true`（ADR 0018 生成 + storage-memory/overview.md 更新）。ADR 0018 が本 PR で生成され、overview.md の「event log のスキーマ詳細」Open question が解決済みとして更新されたことを確認する。ADR 0018 はこの packet の contract（acceptance criteria 冒頭の migration 適用と schema に直接対応）でもあるため省略は不可。