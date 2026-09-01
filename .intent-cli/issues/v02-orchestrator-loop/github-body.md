## Goal

GUIからgoal+packet contractを投入し、worker実装→PR→CI→Reviewer request-update/repair/rereview→人間merge承認→intent-cli closeoutまでをevorch内で継続するorchestrator loopを実装する。finishはcomposite gate+Reviewer承認まで拒否し、idle時にcontinuationを自動dispatchする。

## Why This Slice Exists Now

現行runtimeの`finish`は即受理され、goal durability、evidence gate、idle continuation、review修復、merge approvalがない。実装loopはomo/herdr外部paneと手作業relay/closeoutに依存する。先行v0.2 packetがmessaging/worktree/prompt/skills/rules/compaction/GUIを提供した後、本sliceがそれらを統合してv0.2成功基準を満たすcapstoneになる。

## Current Observed State

- AgentRuntimeはdelegate/send/wait/cancel/list/inspectを持ち、RunConfigはinteractive/nameのみ
- meta `finish`はresultを即受理し、PR/CI/diff/Reviewer gateがない
- run phaseはPending/Running/Waiting/Done/ErrorをEvent BusへemitするがGoalState/idle epoch/review stateはない
- SQLite event/session/message persistenceは既存でdurable stateの基礎になる
- GUI側goal submission/merge approval containerはv02-gui-workbench-restructureが提供する
- GitHub/intent-cliはshell tool経由で利用し、専用bridgeはない

## Accepted Baseline You May Assume

- grill Q6:起点はGUIのみ、durable goal active/paused/complete、composite gate+Reviewer approval、idle continuation
- grill人間承認点: mergeのみ。実装/PR/CI/review/closeoutは自律、GitHub/intent-cliはshell経由
- v02-agent-messaging: durable transcript、steering/reply、crash時は新規run+文脈再構成。厳密reviveはv0.3
- v02-workspace-isolation: runtime所有worktree、writable git、`evorch/task/<run-id>`
- v02-prompt-assembly/skill-loader/project-rules/context-compaction:長時間Orchestrator実行のprompt/context基盤
- ADR 0001:固定workflowではなくinvariant下の動的topology
- ADR 0018:SQLite event sourcing、ADR 0022:親子addressing/nested delegation
- omo prior art: idle event駆動continuation、goal state、completion audit。自然言語停止検出は不採用

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/gui/`, `crates/storage/`, `crates/event-bus/`

Target part: durable goal/state machine、finish gate、idle continuation、review repair、stalled nudge、merge-only approval、shell closeout、headless self-dogfood

## In Scope

- GUI goal+packet/issue+constraintsをrun/sessionへbindしGoalState active/paused/completeを永続化
- restart event replay、pause/resume/complete/cancel/crash state machine
- PR実在+CI green+main差分criteria照合+最新Reviewer approvalのfinish gate
- gate拒否理由とgoal active維持、idle epoch dedupe付きcontinuation prompt
- Reviewer request-update→repair worker→new head→rereview、bounded rounds
- progress/heartbeat/event時刻によるstalled検知、bounded nudge、blocked化
- crash時transcript/stateから新規run文脈再構成（厳密reviveなし）
- approved shell経由のcommit/push/gh PR/CI/intent-cli closeout
- repo/PR/head/gate snapshotにbindされたGUI merge approve/reject、変更時失効
- headless E2Eとfake adapter `--demo`、queued v0.2 unit 1本以上の実self-dogfood evidence

## Out Of Scope

- intent-cli queue polling/seed/publish/issue automation
- 新規evorch CLI entry（起点はGUIのみ）
- GitHub/intent-cli native bridge
- parked runのin-flight snapshot厳密revive
- merge以外の人間approval
- approvalなしmerge、別PR/SHAへのapproval再利用
- orchestratorによるworker worktree直接edit
- 固定workflow DSL

## Standalone Child Issue Contract

GUIから投入したgoal+packet contractをdurable GoalState(active/paused/complete)としてrun/sessionへbindし、event replayで再構成可能にする。`finish`は対象PRが存在しbaseが`main`、headに対するCIがgreen、diffとverificationがpacket acceptance criteriaへ照合済み、最新Reviewerがapprove、の全ANDを満たす時だけ受理する。欠落/古いevidenceでは理由付き拒否してgoalをactiveに保ち、runtime idle epochごとに一度だけcontinuation promptをdispatchする。Reviewer request-update→repair→rereviewとstalled nudgeはbounded、上限時はblocked。実装/commit/push/PR/CI/review/intent-cli closeoutはapproved shellで自律実行し、mergeだけrepo/PR/head/gate snapshotにbindされたGUI approvalを要求する。crash時はtranscript/stateから新規runを起動し厳密reviveはしない。queue polling/publish、新規CLI/native bridgeは実装しない。headlessでrequest-updateを含むend-to-end loopを検証し、queued v0.2 unit 1本以上を実self-dogfoodする。PRは`main`をtargetにする。

## Acceptance Criteria

- GoalStateがSQLiteへ永続化されrestart後再構成できる
- pause/resume/complete/cancel/crashのstate machineがtest済み
- finish gateの全pass/各missing/stale経路がtable-driven test済み
- idle continuationがepoch dedupeされpause/complete/blockedで発火しない
- request-update→repair→rereviewがboundedで、上限時blocked
- stalled→nudge→progress reset/blockedがtest済みで親直接editなし
- merge approvalがrepo/PR/head/gateにbindされ変更/reject/use後失効
- shell経由自律処理とcloseoutを記録しqueue/publishを呼ばない
- crash時新規run文脈再構成、厳密reviveなし
- headless E2Eがreview修復、finish拒否continuation、merge approval、closeoutを完走
- `--demo`決定的再現とqueued unit 1本以上の実self-dogfood evidence
- 新規CLIを追加しない
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`がpass

## Verification

- GoalState/event replay state-machine tests
- finish gate evidence freshness table tests
- idle epoch continuation dedupe tests
- review round/stalled nudge integration tests
- merge approval binding/invalidations/security tests
- shell adapter allow/deny/closeout contract tests
- crash context reconstruction test
- gui HeadlessWorkbench end-to-end packet loop
- fake adapter`--demo`実行と実queued unit self-dogfood記録
- workspace全体test/clippy/fmt/diff check

## Related Links

- intents/evorch/features/orchestration/overview.md
- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/technology/mvp-roadmap.md
- intents/evorch/decisions/0001-no-fixed-workflow.md
- intents/evorch/decisions/0002-role-capability-boundaries.md
- intents/evorch/decisions/0005-headless-kernel-and-gui-separation.md
- intents/evorch/decisions/0018-sqlite-event-sourcing.md
- intents/evorch/decisions/0022-parent-child-tree-addressing-and-nested-delegation.md
- intents/evorch/interviews/grill-v02-loop-foundation.json
- dependencies: `v02-agent-messaging`, `v02-workspace-isolation`, `v02-prompt-assembly`, `v02-skill-loader`, `v02-project-rules`, `v02-context-compaction`, `v02-gui-workbench-restructure`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/orchestration/overview.md` primary。runtime/gui/roadmap supporting、新規intent不要
- ADR candidate: none（既存ADR内。approval tokenが汎用primitive化する場合のみ再評価）
- Diagram candidate: state-machine図をorchestration overviewへ追加
- Docs update: `evorch-gui --help`の`--demo` loop確認手順
- Closeout writeback expected: yes。state/event/gate/continuation/review/nudge/approval/shell/crash/headless/self-dogfood結果を記録

## Guide Reachability (G645)

operator向けgoal loop surfaceを追加する。route: GUI goal実装loopの開始・監視・merge判断 → goal state、gate/review status、continuation、merge approval、closeout result。`no_role_facing_surface: false`。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

作業branchは`evorch/task/<run-id>`規約を用い、child PRはすべて`main`へ直接openする。
