# v02-gui-workbench-restructure Implementation Packet

## Goal

evorch GUIを、t3codeを基準とする「左: project/thread、中央: conversation、右: tabbed surfaces」の既定workbenchへ再構成する。egui_dockの自由配置とframework非依存Workspace Modelを維持しつつ、project trust/allowed directories、thread管理、Agentsのlive一覧・drill-down・複数transcript同時表示、最小Diff tabを実装する。さらにv02-orchestrator-loopが利用するgoal投入とmerge承認の型付きUI containerを用意し、headlessと`--demo`で外部providerなしに検証可能にする。

## Why

現行GUIはAgent/Terminal/Tasksの3 paneと単一transcriptで、runtime wiringの観測には使えるが、複数project/threadを運用し、orchestrator/worker/reviewerを同時監視して実装loopを人間が扱うworkbenchにはなっていない。grill Q9-Q11でt3code型layout、project allowed-directoryとsandbox/project trustの統合、thread管理、Agentsの一覧+drill-down+複数pane、最小Diffのみv0.2が確定した。Q6/Q13でgoal投入と人間merge承認の入口もGUIのみと確定しており、capstone loopより先に器を提供する必要がある。

## Scope

- `workspace-ui`のframework-independent schemaをv0.2 surfaceへ拡張する。既存v0.1 workspaceをmigration/loadでき、egui_dock roundtrip・floating・undockを維持する
- 既定layoutを左sidebar / 中央conversation / 右tabbed surfacesにする。custom saved layoutは尊重し、reset時にv0.2 defaultへ戻す
- project modelを「基準repo/path + allowed-directory set」として定義し、追加/選択/永続化する。canonicalize、重複、存在しないpath、親子関係、trust状態を型付きで扱う
- `cwd != project`を前提にrun/worktreeからproject所属を解決する。v02-workspace-isolationが作る`evorch/task/<run-id>` worktreeは自動許可し、それ以外の外部pathは明示追加/trustなしに許可しない
- 左sidebarでproject配下threadを作成/切替/pin/unpinし、active/paused/running/waiting/done/errorとbranch/worktree indicatorを表示する
- Event Busとv02-agent-messagingのrun/session addressingを用い、agent telemetry view modelをidentity/phase/model/provider/current tool/token usageまで拡張する
- Agents tabのrow選択で中央conversationを対象agent transcriptへdrill-downする。選択解除/元thread復帰も決定的state transitionにする
- `TranscriptModel`をrun/session keyedで複数instance化し、任意agent paneをdock可能にする。既定候補はorchestrator、最新worker、reviewerの3つ。存在しないroleは空placeholderを増やさず利用可能paneだけ配置する
- Diff tabはworking treeと既定base `main`に対するbranch unified diffのみ。既存git_diff/shell境界を利用し、loading/empty/truncated/errorを明示する
- goal submission surfaceはgoal、packet/issue参照、制約を型付きcommandとして発行する。merge approval surfaceはPR、CI、diff、Reviewer結果を表示し、approve/reject commandを発行する。本sliceではloop state machineを実装せずfixture adapterを提供する
- `HeadlessWorkbench` integration testでlayout、project/thread、telemetry、drill-down、transcript分離、Diff、goal/approval commandを検証する
- `evorch-gui --demo`を拡張し、provider/credential不要の決定的scriptで3役agent、project/thread、diff、goal/approval stateを再現する。`--help`へ手動確認手順を記載する

## Out of scope

- v02-orchestrator-loopのgoal durability、finish gate、continuation、review state machine本体
- file tree / search / drag mention
- turn別diff、split/unified切替、ignore-whitespace、任意base branch selector、PR full diff
- Browser preview、PR操作、完全Terminal multiplexer等のt3code全surface再現
- GUIからの`gh pr merge`実行ロジック（本sliceはapproval command containerのみ）
- semantic UI API全体、Level 2/3 UI自己改善
- 複数OS windowの新機能追加（既存layout能力の回帰維持のみ）

## Verification

- workspace-ui unit: v0.2 default tree、schema migration、serialize/deserialize、unknown/duplicate panel、invalid fraction/path/trustのfail-closed
- GUI headless: 左/中央/右layout、project追加/切替、thread作成/切替/pin/status、saved layout roundtrip
- telemetry headless: agent identity/phase/model/provider/tool/token usageのevent反映、中央drill-down、orchestrator/worker/reviewer transcriptのrun-id分離
- Diff fixture: working tree/branch unified、empty/truncated/git error state
- command fixture: goal+packet+constraints submitとmerge approve/rejectが型付きcommandとして一度だけ発行される
- `cargo test -p workspace-ui -p gui`と既存runtime_wiring/workbench/dock testsの回帰
- `cargo run -p gui --bin evorch-gui -- --demo`で外部credentialなし手動経路を実行し、`--help`の手順どおり確認
- workspace全体の`cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/gui-workbench/overview.md` primary。v0.2確定節の直接実装であり新規intent不要
- ADR candidate: decline — egui/egui_dockとheadless分離はADR 0005/0007、project trustはADR 0008で決定済み。schema migration方式がworkspace全体の新原則になる場合のみ別ADRを提案
- Diagram candidate: decline —既存overviewの3領域ASCII diagramを実装結果に合わせて更新すれば十分
- Docs update: `evorch-gui --help`へ`--demo`手動確認手順を追加する。別user guideはこのsliceでは作らない
- Closeout learning: workspace schema、project/thread、Agents addressing、3-pane規則、Diff上限、goal/approval container、headless/demo結果をoverviewへ必須writeback。`write_back_required: true`

- Guide reachability (G645): operatorがGUI workbenchのproject/thread操作とloop開始から左sidebar、Agents/Diff、goal submission、merge approvalへ到達するrouteを宣言する。`no_role_facing_surface: false`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
