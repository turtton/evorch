## Goal

ユーザーメッセージのentry到着時にpre-routingを実行し、ローカルキーワードルール（explicit directキーワード検出）でDirectと高確度判定できた場合のみWorker runを直接起動し、それ以外はOrchestrator runを起動する。ローカルルールで判定不能・低確度・矛盾した場合はOrchestratorと同じモデルによる再分類にフォールバックし、さらに不確かな場合はCoordinatedへfail-safeする。判定結果はRoutingDecision eventとしてイベントバスへ発行する。

## Why This Slice Exists Now

現行のGUI entryはRole::Orchestrator固定でrunを起動しており、ExecutionShapeの事前判定が存在しない。interview new-session.jsonで、デフォルトOrchestrator起動、明示directキーワード時のみWorker直接起動、fail-safeの向きはOrchestrator側、分類モデルはOrchestratorと同一、ローカルルール高確度なら直接起動する2段階方式が確定した。Intent Gateの分類ルールはv02-intent-gate-rulesで型付きポリシーモジュールとして単一ソース化される前提が整ったため、entryでの浅い分類（Layer A）を実装する最初のsliceとして配置する。実行中のWorkerからOrchestratorへの昇格（mid-run escalation）はLayer Bのv02-direct-escalation-handoffが別packetで扱う。

## Current Observed State

- `crates/gui/src/bin/evorch-gui.rs:246-255`の`run()`は`ExecutionPolicy::for_role(Role::Orchestrator)`を固定指定して`AgentRuntime::production`を生成する
- `crates/evorch/src/main.rs`は空の`fn main() {}`でCLI entryは存在しない
- run開始APIは`crates/runtime/src/runtime.rs:290-313`の`delegate_background`/`delegate_background_as_child`で、いずれもRoleを明示指定してRunIdを返す
- Intent Gateの分類指示はOrchestrator prompt内（`crates/runtime/src/prompt/intent_gate.rs:18-48`）のprompt記述のみで、runtimeに型・構造化された分類ルールは存在しない
- entry時点でのローカルキーワード検出、ExecutionShape事前判定、RoutingDecision event発行はどれも未実装

## Accepted Baseline You May Assume

- interview new-session.json確定: デフォルトOrchestrator起動、明示directキーワード時のみWorker起動、fail-safeはOrchestrator側（Coordinatedへ倒す）、分類モデルはOrchestratorと同じ、ローカルルール高確度なら直接Worker起動する2段階方式
- omo（oh-my-openagent）のulwキーワード実装: 正規表現`/\b(ultrawork|ulw)\b/i`、コードブロック・内部メッセージ・既注入directive・スラッシュコマンドを除外、キーワード検出時のみpolicyを適用
- Intent Gateの分類ルールはv02-intent-gate-rulesで型付きポリシーモジュールとして抽出され、コードとprompt両方が同じソースを消費する前提
- v02-workspace-isolation、v02-skill-loader、v02-project-rulesは本sliceより先に着地する依存とする
- ADR 0001: 固定workflowではなくinvariant下の動的topology。pre-routingは固定workflowの導入ではない
- runのevent観測は既存のEvent Bus上で行う（ToolStarted/Completed等と同じ経路）

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/gui/`

Target part: ユーザーメッセージentryでローカルキーワードルールと必要時の同モデル再分類によりExecutionShapeを判定し、CoordinatedのみOrchestratorを起動しDirectはWorkerを直接起動するpre-routing slice

## In Scope

- ローカルキーワード検出器: omoの`/\b(ultrawork|ulw)\b/i`をモデルにしたexplicit directキーワード（例: direct / just）のregex検出。コードブロック、スラッシュコマンド、内部メッセージを除外する
- 2段階分類: ローカルルールで高確度なDirect/Coordinatedは即確定し、判定不能・低確度・矛盾時はOrchestratorと同じモデルによる再分類へフォールバックする
- 分類APIを`crates/runtime`に実装し、GUI entry（`evorch-gui.rs`）から消費する。将来のCLI entryから再利用できるようgui-localに置かない
- 分類ルールはv02-intent-gate-rulesの型付きポリシーモジュールを参照し、prompt内にprompt-onlyの分岐を残さない
- fail-safe: それでも不確かな場合はCoordinatedへ倒してOrchestratorを起動する
- RoutingDecision event（shape、判定理由、使用したルールorモデル）をEvent Busへ発行する
- GUI headlessモードでDirectキーワード有り/無しの両ケースを検証するテスト

## Out Of Scope

- mid-run escalation（実行中Workerの昇格提案、EscalationRequested event、引き上げhandoff）。v02-direct-escalation-handoffの責務
- 新規evorch CLI entryの実装。分類APIがCLIから再利用可能であることは設計要件、CLI binary作成は別slice
- Intent Gateの単一ソース化そのもの。v02-intent-gate-rulesの責務
- ユーザーの手動shape選択UI
- 分類結果に応じたモデル・権限の変更。本sliceは起動Role選択のみに留める
- Orchestrator内部のメッセージ毎Intent Gateの再分類指示の調整。別sliceで扱う

## Standalone Child Issue Contract

GUI entryでユーザーがgoalメッセージを投入した時、起動するrunのRoleを決めるpre-routingを実装する。まずローカルキーワードルールとして、omoのulwパターンをモデルにしたregexによるexplicit directキーワード検出を行う。コードブロック、スラッシュコマンド、内部メッセージは検出対象から除外する。ローカルルールでDirectと高確度判定できた場合のみWorker runを直接起動し、明示キーワードがなければOrchestrator runを起動する。判定不能・低確度・矛盾した入力は、起動予定のOrchestratorと同じモデルによる分類へフォールバックし、それでも不確かならCoordinatedへ倒す。分類ルールはv02-intent-gate-rulesが提供する型付きポリシーモジュールを単一ソースとして読み、prompt内に追加の分岐指示を残さない。分類APIは`crates/runtime`に置き、GUI entryの`evorch-gui.rs`が消費する。判定ごとにRoutingDecision event（shape、理由、ルールまたはモデル）をEvent Busへ発行し、headlessモードでキーワード有り/無しをテスト検証する。PRは`main`をtargetにする。

## Acceptance Criteria

- GUI entryでユーザーメッセージ到着時にpre-routingを実行し、Coordinatedの場合のみOrchestrator runを起動し、Directの場合はWorker runを直接起動する
- ローカルキーワードルールでexplicit directキーワード（例: direct / just）を検出した場合のみWorker直接起動とし、それ以外はOrchestratorを起動する。コードブロック・スラッシュコマンド・内部メッセージ内のキーワードは検出しない
- ローカルルールで判定不能・低確度・矛盾した場合はOrchestratorモデルによる再分類にフォールバックし、さらに不確かな場合はCoordinatedに倒す
- 分類は型付きポリシーモジュール（v02-intent-gate-rules）を参照し、prompt-onlyの分岐を残さない
- RoutingDecision event（shape、判定理由、使用ルールorモデル）をEvent Busに発行する
- GUIのheadlessモードでDirectキーワード有り/無しの両ケースを検証するテストがある
- 分類APIが`crates/runtime`に実装され、GUIがそれを消費する（gui-local実装にしない）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`がpassする

## Verification

- ローカルキーワード検出器のunit test: direct/justキーワード検出、コードブロック除外、スラッシュコマンド除外、内部メッセージ除外、大文字小文字、word boundary
- 2段階分類のtable-driven test: ローカル高確度確定、フォールバック発火、矛盾入力、再分類後も不確か→Coordinated fail-safe
- RoutingDecision event発行のtest: shape、理由、ルールorモデル識別子がevent payloadに含まれる
- GUI headlessテスト: キーワード有りでWorker run起動、キーワード無しでOrchestrator run起動
- promptにprompt-onlyの分類分岐が残っていないことの確認
- workspace全体の`cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/orchestration/overview.md
- intents/evorch/decisions/0001-no-fixed-workflow.md
- intents/evorch/interviews/new-session.json
- crates/gui/src/bin/evorch-gui.rs
- crates/runtime/src/runtime.rs
- dependencies: `v02-intent-gate-rules`, `v02-workspace-isolation`, `v02-skill-loader`, `v02-project-rules`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/orchestration/overview.md` primary。no-fixed-workflowとnew-session interviewをsupportingとする。新規intent不要
- ADR candidate: none。pre-routing 2段階方式はADR 0001の動的topologyの内側で、interview確定済みの運用判断のため
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes。`features/orchestration/overview.md`のv0.2確定節へ、entry pre-routing実装完了（ローカルキーワード＋同モデル再分類）を書き戻す

## Guide Reachability (G645)

本sliceはGUI workbenchのgoal投入surfaceにpre-routing判定を追加する。route: GUI goal投入（pre-routing判定でWorkerまたはOrchestratorを選択）→ role Orchestrator → goal submission UI / run startup。`no_role_facing_surface: false`。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
