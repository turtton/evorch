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

## 受け入れ基準

- Intent Gate が Direct / Coordinated を返し、Coordinated の場合のみ Orchestrator が起動すること
- Orchestrator に mutation tool が無いこと（runtime レベルで拒否されること）
- delegation の理由が説明可能であること

## Related decisions

- [ADR 0001: 固定 workflow を採用しない](../../decisions/0001-no-fixed-workflow.md)
- [ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する](../../decisions/0002-role-capability-boundaries.md)
- [ADR 0022: 親子限定ツリー addressing と can_delegate の Role capability 開放](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md)

## Open questions

- Explorer / Librarian / Reviewer の runtime レベル capability 制限の具体設定（network の role-dependent 扱いの細部）
- Category（quick / deep / high-reasoning / visual / writing / research）のモデル routing との対応表
