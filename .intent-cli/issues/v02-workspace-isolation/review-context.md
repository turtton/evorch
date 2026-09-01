# v02-workspace-isolation Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `WorkspaceMode::{Shared, Isolated}` が `RunConfig` の型付き設定で default Shared になり、既存 callers を壊さないか。stringly typed cwd flag や role 名による暗黙分岐になっていないか
- isolated worktree が runtime ownership で project allowed directories 配下にのみ作られるか。canonicalization / symlink / `..` で許可集合外へ出る path traversal がないか
- branch 名が `evorch/task/<run-id>` に固定され、一意性 / collision が fail-closed か。並列 run が同じ checkout / branch / cwd を共有していないか
- 現行 composition-time 単一 Sandbox / ToolExecutor を run ごとの workspace / policy に安全に接続し直しているか。shared sandbox を path 差替えだけで再利用して race を生んでいないか
- `.git` writable が approval 済み tool call に限定されるか。RoleCapabilities deny、per-tool permission deny、ask 未承認の前に bwrap / shell / git process を起動する fail-open 経路がないか
- worktree `.git` gitfile だけでなく参照先 common git dir の必要 metadata を正しく rw にしつつ、親 repo 全体・他 worktree・credential store を過剰に writable / visible にしていないか
- Worker shell から git add / commit / test remote push が実際に成立するか。runtime 代理 git service、bundle 自動化、commit/push の隠れた bypass を導入していないか
- NetworkAccess → SandboxNetworkMode の fail-closed mapping が維持され、workspace isolation のために network sandbox を全開放していないか
- cleanup が runtime-created resource のみに作用し、user-owned worktree / branch を削除しないか。run 完了直後の cleanup が PR / review に必要な branch を失わせない契約か
- patch mode を実装する場合も branch が既定のままか。merge conflict 自動解決、auto-merge、GUI project management まで scope を広げていないか

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

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が project trust、`.git` rw 最小範囲、approval-before-exec、worktree ownership / cleanup の security boundary を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/agent-runtime-kernel/overview.md`: WorkspaceMode schema / default、worktree manager、allowed dirs validation、branch naming、sandbox per-run composition、`.git` rw mount、cleanup / failure recovery
- `features/orchestration/overview.md`: isolated Worker が `evorch/task/<run-id>` branch で直接 commit / push でき、bundle workaround と runtime proxy git を使わない確定運用

記録が未実施の場合は、v0.2 workspace 計画と実装の drift が残るため知識 writeback 不足として review 所見に残す。
