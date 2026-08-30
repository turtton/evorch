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
3. **mid-run relay**: worker から親 orchestrator への完了前通知・質問・blocked 理由をメッセージとして中継する。現行運用の `[herdr-relay]` 相当で、配送は [agent-runtime-kernel](../agent-runtime-kernel/overview.md) v0.2 計画の配送语义（steering / aside / wake）に従う
4. **review / augment / merge**: 完了結果を review し、不足があれば追加委譲（augment）で補い、worktree の merge で収束する

### 親子限定 messaging とネスト委譲

- subagent 間 messaging は親子限定ツリー addressing に従う（[ADR 0022](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md)）。sibling 間の連携は orchestrator が中継する
- `can_delegate` を Role capability として Worker 等にも開放し、ネスト委譲（worker がさらに subagent を呼ぶ）を正式に許可する。depth cap・自己 spawn 禁止・構想書 §5.1 の乱用防止は ADR 0022 の条件に従う。現行運用で「worker からの再委譲は未検証」とされていた制約を構造的に解消する

### isolated workspace の正式運用

委譲時に `RunConfig.workspace_mode` を指定し、worker ごとに独立した git worktree checkout で実行する運用を正式化する。worktree の作成・破棄・merge は harness が所有し、merge mode は branch（`evorch/task/<run-id>`）が既定。これにより現行運用の痛み（worktree 配置制約、sandbox 下 `.git` read-only に伴う bundle 運用、`/tmp` 非共有）を解消する。runtime 機構の詳細は [agent-runtime-kernel](../agent-runtime-kernel/overview.md) v0.2 計画を参照。

### 参照

oh-my-pi（can1357/oh-my-pi）の設計参照は commit 51f0380 の調査に基づく（参照ファイルの一覧は [ADR 0022](../../decisions/0022-parent-child-tree-addressing-and-nested-delegation.md) の References を参照）。

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
