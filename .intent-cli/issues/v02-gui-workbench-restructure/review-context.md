# v02-gui-workbench-restructure Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- t3code型3領域が「既定layout」であり、egui_dockの自由配置・save/load・floatingを削除して固定レイアウト化していないか
- framework-independent `workspace-ui` modelがlayout/project/thread/panel stateを所有し、egui widget stateやruntime handleを永続modelへ混入していないか（ADR 0005/0007）
- v0.1 workspaceのmigration/loadが可能で、unknown/duplicate panelや不正versionを黙ってresetするfail-openになっていないか
- projectがrepo/path+allowed-directory setとして扱われ、canonicalization/symlink境界を考慮しているか。`cwd == project`を暗黙前提にせず、runtime所有worktreeだけを自動許可し、任意外部pathを自動trustしていないか（ADR 0008）
- thread create/switch/pin/statusがproject/session identityに紐づき、別projectのthread/transcript/diffを混線させないか
- Agents一覧がidentity/phase/model/provider/current tool/token usageをlive更新し、pollingだけでevent orderingを失わないか。provider/usage欠落時は明示unknownであり誤値を生成しないか
- agent drill-downと複数TranscriptModelがrun/session keyedか。orchestrator/worker/reviewer eventが別paneに混線せず、pane close/reopenでsubscription leakや二重描画を起こさないか
- 既定3 paneは利用可能agentに応じて構成され、存在しないreviewer等の空paneでlayoutを埋めないか。任意agent pane追加とegui_dock操作を保持するか
- Diff tabがworking tree/branch unifiedの最小範囲に留まり、untrusted diffをsystem messageとして扱わないか。large output上限/error/empty stateでUIをblockしないか
- goal submission/merge approvalが型付きcommand/event containerに留まり、loop state machineや`gh pr merge`をこのsliceへ混入していないか。approve/rejectが二重発行されないか
- headless testがpixel存在だけでなくstate transition、run-id分離、command payloadを検証するか。`--demo`が外部provider/credential/networkを要求しないか
- file tree、turn別/split diff、Browser/PR full surfaceをv0.2へ広げていないか

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

注: lexical facet contextではlayout persistence、project trust、event addressing、複数transcript分離の意味的接続を証明できない。headless interaction testと上記review focusを主たる判定にする。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required`は`true`。`features/gui-workbench/overview.md`に以下が記録されているか確認する。

- workspace schema/migrationとt3code型default layout
- project allowed-directory、cwd != project、worktree自動許可の実接続
- thread state、Agents telemetry/drill-down、run-keyed複数transcriptと既定3-pane規則
- Diffの取得範囲/上限/error state、goal/merge container、headless/`--demo`検証結果

`evorch-gui --help`の手動確認手順が実際のdemo挙動と一致しない場合もwriteback/docs不足として指摘する。
