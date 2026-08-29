# v01-scaffold Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

Particular focus for this slice.

- crate セットは packet.yaml の acceptance criteria に記載の 11 crate（runtime / event-bus / storage / providers / tools / sandbox / routing / model / config / gui / evorch バイナリ）のみ。architecture.md の他 crate（orchestration / agents / context / diagnostics 等）を作成して scope を広げないこと。
- flake.nix は「既存 devShell への Rust toolchain 追加」に留め、flake 構造の再構築や複数 devShell 化を行わないこと。
- ADR 0016 と architecture.md の Open questions 更新（G461 writeback）が本 PR または明示的な follow-up packet として含まれることを確認する。

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

`closeout_learning.write_back_required` は `true`（ADR 0016 生成 + architecture.md Open questions 更新）。ADR 0016 が本 PR で生成され、architecture.md の「crate 分割の初期 granularity」が解決済みとして更新されたことを確認する。ADR 0016 はこの packet の contract（acceptance criteria 最後の項目）でもあるため省略は不可。