# v01-tool-layer Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- 統一 `Tool` trait（name / schema / execute / 結果正規化）が v01-scaffold の crate 構成に収まっているか
- read / edit / grep / shell / git_diff の5ツールのみ実装し、write / glob / bash / code-intel / MCP 等の範囲外 tool を実装していないか
- edit が一時ファイル + rename で atomic write しているか、ADR 0008 の制御マーカー（`<system-reminder>` 等）エスケープが tool result へ適用されているか
- shell が Shell/PTY 分離（非 interactive = tokio::process::Command、interactive = portable-pty）を満たしているか
- 存在しない path に対して typed error を返すか
- tool 実行結果が event stream へ emit されるか（v01-event-stream の tool_result イベントへ接続）
- sandbox / approval / credential の実装を持ち込んでいないか（v01-sandbox-approval の責務）

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

- `features/tools-sandbox/overview.md` への v0.1 標準5ツール（read / edit / grep / shell / git_diff）確定、edit への制御マーカー エスケープ適用、tool result の event stream への emit 記録

記録が未実施の場合は知識 writeback が不足している旨を review 所見に残す。