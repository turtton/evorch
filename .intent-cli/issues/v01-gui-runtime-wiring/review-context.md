# v01-gui-runtime-wiring Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `evorch-gui` と `AgentRuntime` / `EventPump` が同一 `Arc<EventBus>` を共有し、製品経路から `EmptyAgentSource` が除去されているか。
- Tokio runtime / spawned AgentRun / event bridge の lifetime が native GUI 終了まで安全に維持され、GUI close 時に不要な thread/task leak を作らないか。
- `AgentSummary` の name / model が固定 GUI label ではなく runtime の各 RunEntry に対応する実 identity であり、role と name を意図せず同一視していないか。
- runtime が provider routing を再実装していないか。`AgentModel` 境界を維持しつつ、選択済み model identity を明示的に受け渡しているか。
- tasks pane の状態更新で name / role / model が失われず、未知 run event の refresh も実 runtime source を読むか。
- 自動テストが DTO mapping と EventBus flow を覆い、egui native 描画の限界は再現可能な手動手順・期待結果で補っているか。
- `crates/runtime/examples/orchestrator_demo.rs` の wiring / scenario に不要な挙動変更がなく、新規 pane・orchestrator logic・GUI redesign に踏み込んでいないか。

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

本 packet は `closeout_learning.write_back_required: true`。`intents/evorch/features/gui-workbench/overview.md` と `intents/evorch/technology/mvp-roadmap.md` に、製品 GUI / runtime の共有 EventBus 起動 lifecycle、AgentSummary identity 境界、外部 provider 無しの手動確認手順が書き戻されていることを確認する。ADR / diagram / 新規 intent node は不要であり、その非更新はブロッキングにしない。

## Guide Reachability (G645)

route 宣言（`guide workflow task implementation-loop` → role `implementation` → 実 AgentRuntime session と live tasks pane を持つ evorch GUI app）が closeout で参照可能になっていることを確認する。
