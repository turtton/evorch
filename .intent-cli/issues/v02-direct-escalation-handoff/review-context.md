# v02-direct-escalation-handoff Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- terminal-before-transfer ordering: workspace 譲渡と新 run 起動が旧 run の terminal 保証完了より後か。新規 tool call 受付停止、in-flight tool の完了/abort、pending send/wait_reply/inbox の close、`AgentRunPhase` の Done/Error 遷移がすべて譲渡前に完了しているか。譲渡を先行させる経路や、terminal 化を await しない fire-and-forget 化がないか
- exclusive workspace ownership proof: 譲渡後に旧 run から workspace へ書き込める経路が残っていないか。排他性が `OwnedWorktree` / `WorktreeManager` の所有権移動で構造的に表現され、同時 writer が存在し得ないか。約束事コメントだけで済ませていないか
- ADR 0022 compliance: 新 run が旧 run の子ではなく root run か。`delegate_background_as_child` 等の child 生成経路を流用して逆向き delegation edge を作っていないか。新 run が `source_run_id` を保持するか
- EscalationRequested event emission: source_run_id、memo 概要、新 run_id を含む event が event bus に発行され、wire format の round-trip test があるか。v02-orchestrator-loop の observability と同じ bus に乗っているか
- meta-op 登録経路: 専用 meta-op が `crates/runtime/src/meta/mod.rs` の文字列 match dispatch に handler module + match arm で登録され、dispatch を bypass する呼出し経路がないか。args 検証が既存の fail-closed parse pattern に倣っているか
- EscalationMemo の完全性: 確定スキーマ（source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state(dirty files/summary), escalation_reason, suggested_next）の全フィールドがあるか。`files_touched` が `Vec<PathBuf>` か。workspace_state が dirty files 一覧と summary の両方を持つか
- push 型安全網: 停滞検知（連続 edit 失敗 / 反復書き換え / tool call 数閾値）の昇格提案 event が agent の自己申告に依存しないか。閾値が調整可能か。提案が観測 event に留まり、自動強制昇格や旧 run の勝手な kill に繋がっていないか
- scope 遵守: pre-routing 判定、Orchestrator loop 本体の gate/review/merge logic、workspace 変更の自動 commit/merge、in-flight snapshot の厳密 revive を持ち込んでいないか

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

注: facet-check は terminal ordering、workspace 排他所有権、event 発行を証明しない。ordering / 所有権 / event payload の test と上記 review focus を主たる判定にする。

## Knowledge Writeback Expectation (G461)

If the packet's `closeout_learning.write_back_required` is `true`, confirm the
expected intent-tree / ADR / diagram / docs writeback landed in this PR or was
captured as a follow-up packet. If the packet declined all knowledge maintenance,
that is acceptable. Note it rather than blocking.

本 packet は `write_back_required: true`。escalation meta-op・EscalationMemo スキーマ・workspace 排他譲渡の実装完了が `intents/evorch/features/orchestration/overview.md` の v0.2 確定節へ反映されていることを確認する。未反映なら follow-up packet として捕捉されているかを確認し、どちらもない場合は writeback 不足として指摘する。
