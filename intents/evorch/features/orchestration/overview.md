# Feature: Orchestration（動的オーケストレーション）

[features 一覧](../) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md) / [mission](../../identity/mission.md)

## 概要

workflow は固定しない。Agent の責任・認知モード・権限・実行ポリシーを固定し、Orchestrator が依頼に応じて動的に topology を構築する。

## 要件

- **Intent Gate**: task type / required capabilities / mutation allowed? / scope / uncertainty / expected output / completion criteria / likely need for delegation を抽出する。workflow は決めない
- **Execution Shape**: Direct（単純な質問・局所的修正）または Coordinated（複雑な調査・実装・並列探索）だけを決める
- **Role 分離**: Orchestrator / Explorer / Librarian / Oracle / Planner / Reviewer / Worker / Multimodal を capability boundary として分離する。cognitive isolation（生成と独立レビューの分離）を徹底する
- **Orchestrator の tool 制限**: delegate / delegate_background / send_message / wait / cancel / list_agents / inspect_agent / read / grep / git_diff / compact / finish のみ。write / edit / apply_patch / arbitrary shell / git commit は持たせない
- **DelegationValue**: Expertise / Parallelism / ContextIsolation / IndependentReview / DifferentInformationSource / Scale。「複雑だから delegate」ではなく delegation に具体的価値を要求する
- **Agent の5軸分解**: Agent Instance = Role + Category + Skills + Execution Policy + Route Policy

## v0.1 orchestration runtime の実装確定（2026-08-30）

Orchestrator / Explorer / Worker / Reviewer の 4 role 実行と background agent が `crates/agents/` + `crates/runtime/` にコード確定（PR #16、issue #7）。capability boundary は ADR 0002 行列を `RoleCapabilities` で runtime レベル強制。Orchestrator の delegate / delegate_background / send_message / wait は meta 操作として ToolUse dispatch で処理され、event stream で観測可能。詳細は [agent-runtime-kernel](../agent-runtime-kernel/overview.md) の確定節を参照。

v0.1.1 確定（PR #20、issue #19）: role の network capability は `crates/runtime/src/network.rs` の写像から `BwrapConfig.allow_network` へ伝播する。`build_sandbox(&ExecutionPolicy, workspace)` が composition seam で、production composition root からの呼び出しは `v01-secure-tool-composition-root` / `v01-gui-runtime-wiring` が消費する。allow は full-open（destination filter 非対応、selective egress は v0.2）。

## v0.2 方向: 委譲ループ protocol の正式化（grill subagent-internalization 確定、2026-08-30）

### 動機: herdr-opencode-loop の内製化

現行の [herdr-opencode-loop](../../../.opencode/skill/herdr-opencode-loop/SKILL.md) 運用は、lead ↔ worker を別 pane の外部 opencode セッションとして起動し、pane prompt 送信と `[herdr-relay]` リレー、手作業の worktree 配置、sandbox 制約下の bundle 受け渡しで実装ループを回している。evorch はこの運用を **external pane 運用から harness 内の tool で完結する委譲ループへ内製化** する。

### 委譲ループ protocol

1. **contract**: 親 orchestrator が goal + context の契約を作る（現行運用の `.opencode/<slice>-contract.md` 相当を run の入力として正式化）
2. **background delegation**: `delegate_background` で worker run を起動する
3. **mid-run relay**: worker から親 orchestrator への完了前通知・質問・blocked 理由をメッセージとして中継する。現行運用の `[herdr-relay]` 相当で、配送は [agent-runtime-kernel](../agent-runtime-kernel/overview.md) v0.2 計画の配送语义（steering / aside / wake）に従う。実装確定（PR #48）: mid-run relay は lifecycle 完了通知と独立した durable channel（`AgentMessage` event → storage transcript）を使う
4. **review / augment / merge**: 完了結果を review し、不足があれば追加委譲（augment）で補い、worktree の merge で収束する

### 親子限定 messaging とネスト委譲

- subagent 間 messaging は親子限定ツリー addressing に従う（[ADR 0022](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md)）。sibling 間の連携は orchestrator が中継する
- `can_delegate` を Role capability として Worker 等にも開放し、ネスト委譲（worker がさらに subagent を呼ぶ）を正式に許可する。depth cap・自己 spawn 禁止・構想書 §5.1 の乱用防止は ADR 0022 の条件に従う。現行運用で「worker からの再委譲は未検証」とされていた制約を構造的に解消する

### isolated workspace の正式運用

委譲時に `RunConfig.workspace_mode` を指定し、worker ごとに独立した git worktree checkout で実行する運用を正式化する。worktree の作成・破棄・merge は harness が所有し、merge mode は branch（`evorch/task/<run-id>`）が既定。これにより現行運用の痛み（worktree 配置制約、sandbox 下 `.git` read-only に伴う bundle 運用、`/tmp` 非共有）を解消する。runtime 機構の詳細は [agent-runtime-kernel](../agent-runtime-kernel/overview.md) v0.2 計画を参照。

### 参照

oh-my-pi（can1357/oh-my-pi）の設計参照は commit 51f0380 の調査に基づく（参照ファイルの一覧は [ADR 0022](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md) の References を参照）。

### ループ継続保証・merge 承認の確定（grill grill-v02-loop-foundation、2026-09-02）

omo（oh-my-openagent 4.19.4 調査）の /goal + continuation 機構を踏襲し、委譲ループの完結をシステムで保証する:

- **goal 固定**: run に goal を紐付け、durable な goal state（active / paused / complete）を保持する
- **finish gate**: orchestrator / worker の `finish`（完了宣言）は composite gate（PR 実在 + CI green + diff の成功基準照合）と Reviewer 承認を満たさなければ拒否し、run を継続させる。omo の「idle イベント駆動 continuation dispatch」（todo / goal / boulder 未完了時に continuation prompt を自動注入）に相当する機構を runtime が持つ
- **review 往復**: Reviewer run の指摘は request-update として worker へ差し戻し、rereview まで orchestrator が回す（現行運用の lead 手作業の内製化）
- **人間承認点は merge のみ**: 実装・PR 作成・CI 確認・review 往復・closeout 記録 (intent-cli は shell 経由) は自律。`gh pr merge` だけ GUI approval で人間に求める（現行ループと同じ安全水準）
- **起点は GUI の goal 投入のみ**: CLI（crates/evorch main.rs）は新設しない。検証は gui crate の headless モードで行う（ADR 0005 の分離と一致）
- **停滞検知**: worker の無応答・エラー停滞を検知して追加指示（促し）を送る。lead が直接修正しない規律（herdr-opencode-loop 運用）は維持
- **Intent Gate との統合**: 既存構想の Intent Gate（Direct / Coordinated 分類）に、omo 式の分類表（explain / implement / look into / broken / refactor 等）と利用可能 agent / skill から動的生成される keyTriggers を実装する（詳細は v02-prompt-assembly / v02-orchestrator-loop packet）

## v0.2 prompt assembly / routing の実装確定（issue #49、PR #50、2026-09-02）

- **config schema**: `[agents]` セクション = `AgentsConfig{orchestrator,explorer,worker,reviewer: RoleBindingConfig{logical_model,preset,generation{temperature,top_p,max_tokens,reasoning_effort},categories}}`。カテゴリは固定 6 種（quick / deep / high-reasoning / visual / writing / research）で検証。`binding_for(role,category)` は per-field category-beats-role マージで `ResolvedAgentBinding` を返す。config version は 2 のまま（additive）。
- **logical model → routing 接続**: binding の `logical_model` が `LogicalModelId` となり Router が `(profile, model_id)` へ解決。同一 model_id 異 profile は `(profile, concrete model)` pair identity により別 fallback 候補として順選択。`FallbackTriggered` に `from_model` を追加し `FallbackAxis{Provider,Model,Both}` で provider/model fallback を区別。SCHEMA_VERSION は 1 維持。
- **prompt assembly 順序**: role baseline → model-family optimization → category overlay → Orchestrator Intent Gate → preset/user appendix の deterministic 固定順。byte-identical は golden test が固定。Intent Gate は Orchestrator のみ（8 分類項目 + Direct/Coordinated + mutation 非持越）。dynamic keyTriggers は `AvailableAgent`/`AvailableSkill` metadata → `triggers_from_availability`（昇順ソート、横断 dedup、agent 優先、空集合有効）。
- **model-family 判定（`ModelFamily::classify`）**: claude 含有→Claude / o1,o3,o4 prefix→OpenAiReasoning / gpt-5 prefix→Gpt5 / gemini 含有→Gemini / kimi 含有→Kimi / 他 Unknown→family-generic（fail-safe）。
- **preset**: bundled（`crates/config/assets/presets/{role,family,category}-*.md` 16 件、`include_str!`）+ user override（`<user_config_dir>/presets/`）の 2 層。resolver read-only、name=`[a-z0-9-]{1,64}`、≤64KiB、UTF-8。category スコープ appendix（`categories.<name>.preset`）はロールレベル appendix に勝つ。

workspace 隔離が実装確定（PR #52）: isolated mode は runtime 所有 worktree（`<repo>/.evorch/worktrees/<run-id>`、branch `evorch/task/<run-id>`）で、worktree rw は runtime 確保、worker は承認済み tool call 内で直接 git add / commit できる（bundle / runtime 代理 commit push は不採用）。cleanup は runtime が worktree のみ決定的に削除し、branch は merge deliverable として保持する。

## v0.2 pre-routing / escalation の確定（2026-09-03）

Intent Gate の判定ロジックを prompt 内固定文字列から型付きポリシーモジュールへ抽出し、entry での pre-routing（Direct / Coordinated 判定）と Direct→Orchestrator escalation を正式化する。

- **Intent Gate のコード化**: `ExecutionShape`、8 分類軸、Direct/Coordinated 判定結果、mutation 非持越ルールを `crates/runtime/src/prompt/intent_gate.rs` の固定プロンプト文字列から、独立した型付きポリシーモジュールへ移行する。既存 prompt はそのモジュールから生成し、出力を変えない golden test を持たせる。
- **pre-routing (Layer A)**: entry でユーザーメッセージを受け取った際、ローカルキーワードルール（omo `ulw` 型の regex injector）で「明らかに単一作業」を判定し、該当すれば Worker 直接起動、該当しなければ Orchestrator 起動へ分岐する。デフォルトは Orchestrator 起動、明示 direct キーワード（例: `direct` / `just`）時のみ Worker 直接起動。fail-safe は Orchestrator 側。
- **escalation (Layer B)**: Worker 直接起動中に「これは複数モジュールまたぐ / 依存調査が必要 / 並列化したい」と気づいた場合、専用 meta op で `EscalationMemo`（構造化スキーマ: `source_run_id`, `original_request`, `findings`, `files_touched: Vec<PathBuf>`, `blockers`, `workspace_state(dirty files/summary)`, `escalation_reason`, `suggested_next`）を記録して旧 run を terminal にし、新しい Orchestrator root run を起動する。workspace は引き継ぐ（旧 run terminal 保証後に排他的に譲渡）。
- **モデル**: pre-routing で分類に使うモデルは、起動予定の Orchestrator と同じモデルを使用する。ローカルルールで高確度な場合は直接 Worker 起動、低確度・矛盾時のみ Orchestrator モデルで再分類する 2 段階方式。
- **観測**: pre-routing 判定結果と escalation イベントは runtime event で記録し、`v02-orchestrator-loop` の observability（ToolStarted/Completed 同様の event bus）に乗せる。具体的には RoutingDecision event（Direct/Coordinated, 判定理由, 使用ルール or モデル）と EscalationRequested event（source_run_id, memo 概要, 新 run_id）を発行。
- **Intent Gate との関係**: 再分類の指示は Orchestrator prompt から削除し、選択結果を検証する説明に留める。ルールは単一ソース化し、routing prompt と Orchestrator prompt の両方をレンダリングする形にする。

- **v0.2 context compaction（PR #64）**: model-visible context の切替は provider request 層で checkpoint+recent のみに制限され、AgentMessage 配送（durable channel、PR #48）・project rules（synthetic System、PR #62）と独立に重ねられる。orchestration 面で意識するのは compaction 後も goal / 委譲結果の参照可能性が transcript（storage）側で保たれること。詳細は [agent-runtime-kernel overview](../agent-runtime-kernel/overview.md) の「v0.2 context compaction の実装確定」

**実装確定（issue #69、PR #70、2026-09-03）**: Intent Gate 分類ルール（8 分類軸・タスク種別判定表・ExecutionShape 判定・mutation 非持越ルール）は `crates/runtime/src/prompt/intent_gate_policy.rs` の型付きポリシーモジュールへコード化済み。prompt 生成は同モジュールからのレンダリングに統一（単一ソース化）し、Orchestrator 向け本文は entry pre-routing の選択結果を検証する枠組みに更新済み。routing prompt（Layer A）と Orchestrator prompt の両方を同一型定義から生成する API が公開され、出力は byte-identical golden test で決定論性を固定。

## v0.2 確定（PR #76）: direct escalation handoff

- Worker 専用 meta-op `escalate` が `EscalationMemo`（source_run_id / original_request / findings / files_touched / blockers / workspace_state / escalation_reason / suggested_next）を凍結・記録し、旧 run を `Done("escalated")` へ terminal 化。`OwnedWorktree` は値移動で新規 Orchestrator root run（parent None、ADR 0022 準拠・child 経路不使用）へ排他譲渡し、memo を初期 context として起動する。`EscalationRequested`（source_run_id/new_run_id/memo summary）を LifecycleEvent として発行
- 安全網: runtime per-run detector が連続 edit 失敗（既定 3）・同一ファイル反復書き換え（既定 5）・tool call 閾値（既定 200）で観測専用 `EscalationProposed`（latch 1 回、`EscalationSettings` で調整可能）を発行。自動昇格・履歴注入は行わない
- feasibility 確定: 実行中 tool は run 内逐次実行により構造的に in-flight 不在（batch 先行完了・後続不実行、abort 不導入）。Shared mode は逐次保証、Isolated は所有権 move。escalated run は run_result == None（契約としてテスト固定）
- follow-up 候補: EscalationSettings の config crate 配線、GUI での escalation event 描画、handoff 後の未 commit 変更の child run 継承方針、memo store の上限/redaction 方針

## 受け入れ基準


## Related decisions

- [ADR 0001: 固定 workflow を採用しない](../../decisions/0001-no-fixed-workflow.md)
- [ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する](../../decisions/0002-role-capability-boundaries.md)
- [ADR 0022: 親子限定ツリー addressing と can_delegate の Role capability 開放](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md)

## Open questions

- Explorer / Librarian / Reviewer の runtime レベル capability 制限の具体設定（network の role-dependent 扱いの細部）
- Category（quick / deep / high-reasoning / visual / writing / research）のモデル routing との対応表


## v0.2 確定（PR #72、issue #71）: entry pre-routing

GUI の goal 投入時に Orchestrator 起動へ進む前に Execution Shape を事前判定する（entry pre-routing）。`crates/runtime/src/entry_routing/` の `EntryRouter` が 2 段階で分類する:

1. **ローカルキーワードルール**: fenced/inline code・slash command 行を除外した goal 本文を word boundary・case-insensitive で走査。direct 系キーワード（`direct`/`just`）のみ検出→Direct（Worker run を直接起動）、未検出→Coordinated（Orchestrator 起動）、direct 系と coordination 系の混在または分類対象テキスト無し→不確実。
2. **同一モデル再分類**: 不確実時のみ、起動予定の Orchestrator と同一モデルへ Intent Gate 本文と `ExecutionShape:` マーカー報告指示を送って再分類する（`AgentRuntime::entry_router()` が runtime 内部のモデルを構造的に再利用するため同一性が保証される）。マーカーが一意に parse できない場合・モデルエラー時は Coordinated に fail-safe。

全判定経路で `LifecycleEvent::RoutingDecision { shape, reason, source: LocalRule{rule} | Model{model} }` event を発行し、entry 判断を event stream で観測可能にする。GUI 側は `RuntimeCommandSink`（`crates/gui/src/runtime_sink.rs`）が `SubmitGoal` を pre-routing 経由の background run 起動（`goal-N` 採番、Direct→Worker / Coordinated→Orchestrator の role 選択）へ接続する。shape 毎に runtime を再構築するのではなく共有 runtime 上で run の role を選択する方式。

## 検証・ゲート実績
6 commits 17 files +1802/-7 / CI 系全 PASS（146 suites 0 failed、otel-exporter feature 両面）/ Reviewer Gate APPROVED_WITH_NOTES（blocker 0、note 2）。canonical: claim/result-summary/complete 全 applied、issue #71 intent-pr-created。**queue-state linked_pr 同期は sandbox RO で失敗 → lead 側で closeout-plan --write-recovered-linkage が必要**。

## v0.2 確定（PR #74）: orchestrator loop 内製

goal 投入から PR・review・merge 承認まで継続する durable orchestration loop を `crates/runtime/src/orchestration/`（supervisor/gate/approval/continuation/stall/review/closeout/ledger/delivery/shell_delivery/types/registry/prompts）に実装。GoalState（active/paused/complete）は SQLite event sourcing で durable、restart 後に session/goal を再構成。finish は PR 実在・CI green・packet 照合・最新 Reviewer approval の composite gate でのみ受理（欠落は理由付き拒否+goal active 維持）。gate 未充足 idle で continuation prompt を自動 dispatch（同一 idle epoch で二重発火しない）。review 修復往復は config bounded、stalled は event 時刻+progress signal で判定して nudge→blocked。実装・commit/push・PR・CI・review・closeout は approved shell tool 経由（専用 bridge/新 CLI なし）。merge のみ人間 approval 必須で、approval は PR/head SHA/gate snapshot に bind され変化で失効、reject は goal active continuation へ戻す。crash 復旧は transcript+durable state から新規 run 再構成（厳密 revive なし）。gui headless fixture で goal→worker→PR/CI→review→repair→approval→merge→closeout を完走検証、--demo は決定的 adapter で再現。実バイナリ検証で continuation cascade バグを発見・修正済（1015b23）。

**残務（self-dogfood）**: queued v0.2 unit 1 本以上を実 loop で消費した evidence を closeout writeback と mvp-roadmap.md に記録すること（本 loop の実運用初回検証。closeout 時義務）。
