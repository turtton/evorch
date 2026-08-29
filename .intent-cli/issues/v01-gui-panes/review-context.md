# v01-gui-panes Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific focus

- Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer の層構造を守っているか（GUI framework が architecture の中心にならない）。Workspace Model が `crates/workspace-ui/` の framework 非依存データとして検証可能か
- egui_dock の binary split 制約に対し、初期3 pane が nested split で構成されているか（ADR 0007 consequence）
- offscreen レンダリング（ADR 0009）でフレームを capture できること。TUI を製品として作っていないこと
- semantic UI introspection（v0.5）や Cost / Cache Inspector 等の高度 pane（v0.2 以降）に踏み込んでいないか
- テストが headless（offscreen / UI Event Bus へのイベント注入）で完結していること

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

本 packet は `closeout_learning.write_back_required: false` であり、intent-tree / ADR / diagram / docs の必須書き戻しは無い（decline）。closeout で egui_dock の nested split 3 pane の具体構成と offscreen capture の確定方式が学習として回収されることだけ期待する。必須書き戻しが無いこと自体はブロッキングにしない。

## Guide Reachability (G645)

本 packet は route を宣言している（guide workflow task implementation-loop → role: implementation → target_surface: the evorch GUI app）。closeout 時にこの route が `stalled-work` 等のガイド機構から参照可能であることを確認する。