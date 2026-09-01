## Goal

evorch GUIを、左project/thread sidebar・中央conversation・右tabbed surfacesのt3code型既定workbenchへ再構成する。project trust、thread管理、Agents live/drill-down/複数transcript、最小Diffを実装し、orchestrator-loop用goal投入/merge承認containerをheadlessと`--demo`で検証可能にする。

## Why This Slice Exists Now

現行GUIはAgent/Terminal/Tasksと単一transcriptでruntime観測はできるが、複数project/threadとorchestrator/worker/reviewerを扱う実装loopのworkbenchではない。grill Q9-Q11でlayout、project allowed-directory、thread管理、Agents 3-way live、最小Diffが確定し、Q6/Q13でloop起点とmerge承認はGUIのみと確定した。capstone loopが使う人間向けsurfaceを先に提供する。

## Current Observed State

- `WorkbenchState`はEventPump、単一TranscriptModel、TasksModel、TerminalBuffer、DockStateを統合する
- `workspace-ui` schema v1はframework-independent Split/Tabs/Window/Panelを持ち、既定はTasks+Agent+Terminal
- `PanelKind`はAgent/Terminal/Tasksのみで、v0.2 surfaceとmigrationが未実装
- `TasksModel`はrun_id/name/role/phase/modelのみ。provider/current tool/token usageはない
- `TranscriptModel`は全Message/Tool eventを単一streamへ反映し、agent/run別addressingがない
- `HeadlessWorkbench`とruntime wiring/dock roundtrip test、外部provider不要の`--demo`が既存
- v02-agent-messagingとv02-workspace-isolationがtranscript addressingとruntime所有worktreeを提供する前提

## Accepted Baseline You May Assume

- ADR 0005: GUIはAgent Kernel/Event Bus/Workspace Modelの薄いrenderer、headless検証可能
- ADR 0007: egui+egui_dockを採用し自由配置を保持
- ADR 0008: project trustはv0.2実装項目。allowed pathをfail-closedで扱う
- grill Q9: project=repo/path+allowed-directory set、cwd!=project、worktree自動許可、thread create/switch/pin/status、t3code型default
- grill Q10: Agents identity/phase/model/provider/tool/token usage live + central drill-down + orchestrator/latest worker/reviewer約3pane
- grill Q11: v0.2 Diffはworking tree/branch unifiedのみ。完全版はv0.3
- gui-workbench overview: goal submissionとmerge approvalはGUIが器を提供しloopが利用

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/`, `crates/workspace-ui/`, `crates/config/`

Target part: t3code型default workbench、project/thread model、Agents live/複数transcript、最小Diff、goal submission/merge approval container

## In Scope

- workspace-ui schema拡張/migrationと左・中央・右のv0.2 default layout
- egui_dock move/resize/undock/floating/save/load維持
- project追加/選択/永続化、repo/path+allowed-directory set、cwd!=project解決、worktree自動許可
- thread create/switch/pin/unpin/status/branch/worktree indicator
- Agents identity/phase/model/provider/current tool/token usage live list
- agent選択による中央transcript drill-downとrun-keyed複数transcript pane
- 既定orchestrator+latest worker+reviewer最大3pane
- working tree/branch unified Diffのloading/empty/truncated/error state
- goal+packet/issue+constraints submission commandとmerge approve/reject command container
- HeadlessWorkbench integrationと外部credential不要`--demo`/`--help`

## Out Of Scope

- orchestrator-loop state machine、goal durability、finish gate、continuation、review往復
- file tree、turn別/split diff、whitespace制御、任意base selector、PR full diff
- Browser/PR/Terminal等t3code全surfaceの再現
- GUIからの`gh pr merge`実行
- semantic UI API全体、Level 2/3自己改善

## Standalone Child Issue Contract

`crates/workspace-ui`と`crates/gui`を拡張し、新規/初期workspaceの既定を左project/thread・中央conversation・右tabbed surfacesにする。egui_dockの自由配置と既存v0.1 layout migrationを維持する。projectはrepo/path+allowed-directory setとして永続化し、cwd!=projectを扱い、v02-workspace-isolationのruntime所有worktreeのみ自動許可する。thread create/switch/pin/status、Agentsのidentity/phase/model/provider/tool/token usage live list、中央drill-down、run-keyed複数transcript（既定orchestrator/latest worker/reviewer最大3pane）、working tree/branch unified Diffを実装する。goal+packet/constraints submissionとmerge approve/rejectは型付きcommand containerとして提供しloop本体やmerge実行は入れない。HeadlessWorkbenchと外部credential不要`--demo`で全主要state transitionを検証する。完全版Diff/filesは実装しない。PRは`main`をtargetにする。

## Acceptance Criteria

- v0.2 default layoutとegui_dock save/load/floatingのheadless roundtripがpass
- project allowed-directory/cwd!=project/worktree自動許可と外部path拒否がtest済み
- thread create/switch/pin/status/branch-worktree indicatorがheadlessで検証済み
- Agents全telemetryがlive更新し、中央drill-downできる
- orchestrator/worker/reviewer複数transcriptが同時表示されevent混線しない
- Diffがworking tree/branch unifiedを表示しempty/truncated/errorを安全に扱う
- goal submissionとmerge approve/rejectが型付きcommandとして一度だけ発行される
- `--demo`がproject/thread/3役/Diff/goal/approvalをcredentialなしで再現し`--help`に手順がある
- 既存v0.1 layoutをloadでき、不正schema/pathをfail-closed拒否
- 完全版Diff/filesを追加しない
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`がpass

## Verification

- workspace-ui schema/default/migration/validation unit tests
- HeadlessWorkbenchのlayout/project/thread/Agents/drill-down/3 transcript/Diff/command integration
- existing runtime_wiring/workbench/dock roundtrip regression
- `cargo run -p gui --bin evorch-gui -- --demo`実行と`--help`手順確認
- workspace全体test/clippy/fmt/diff check

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/technology/mvp-roadmap.md
- intents/evorch/decisions/0005-headless-kernel-and-gui-separation.md
- intents/evorch/decisions/0007-gui-framework-egui-first.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/interviews/grill-v02-loop-foundation.json
- dependencies: `v02-agent-messaging`, `v02-workspace-isolation`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/gui-workbench/overview.md` primary。新規intent不要
- ADR candidate: none（ADR 0005/0007/0008内）
- Diagram candidate: none（overview既存図を更新）
- Docs update: `evorch-gui --help`の`--demo`確認手順
- Closeout writeback expected: yes。schema/project/thread/Agents/Diff/container/headless結果をoverviewへ記録

## Guide Reachability (G645)

operator向けworkbench surfaceを追加する。route: GUI workbenchのproject/thread操作と実装loop開始 → 左sidebar、Agents/Diff tabs、goal submission、merge approval。`no_role_facing_surface: false`。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

作業branchは`evorch/task/<run-id>`規約を用い、child PRはすべて`main`へ直接openする。
