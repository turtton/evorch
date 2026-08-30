# v01-sandbox-approval Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- 本 packet の最初のタスク（Linux sandbox 第一実装の選択）が完了し、ADR 0021（bwrap 採用）が記録されているか。bwrap 実行不可環境での fail-closed 方針が ADR で扱われているか
- approval 層が auto-allow / ask / deny を policy で分類し、deny では実行されないか。ask は承認応答を待つか
- 二層分離: 承認しても sandbox 外の操作が実行されないか（approval と OS enforcement が分離しているか）
- credential 隔離: agent プロセス・子プロセス・env へ credential が渡っていないか（keychain 優先 / 0600 fallback）
- network egress 既定 deny: allowlist（provider endpoint）のみ通るか
- sandbox 対象を dangerous tool に限り、全ての tool 実行を包んでいないか（範囲の確認）
- macOS / Windows sandbox や untrusted mode（v0.3）を実装していないか。マーカー エスケープは tool-layer の責務
- approval UI の GUI 実装を持ち込んでいないか（v01-gui-panes 側の責務。本 slice は要求/応答 API）

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

注: `intent-cli intent facet-check` は lexically にしか確認しない（no_facet_data は語彙の一致有無を報告し、意味検証はしない）。上記 Slice-specific review focus が意味アライメントの主たる確認点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が記録されているか確認する（この PR 内 or follow-up packet として）。

- ADR 0021（Linux v0.1 sandbox 第一実装: bubblewrap（bwrap）採用）の新設
- `features/tools-sandbox/overview.md` の Open question『Linux sandbox の第一実装の選択（Landlock vs bwrap）』の解消記録

記録が未実施の場合は知識 writeback が不足している旨を review 所見に残す。