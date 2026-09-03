## Goal

Worker 直接起動（Direct）run が実行途中に「手に負えない」と判断した際、構造化 `EscalationMemo`（source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state(dirty files/summary), escalation_reason, suggested_next）を記録して旧 run を terminal 状態にし、workspace を排他的に譲渡したうえで新規 Orchestrator root run を起動する専用 escalation meta-op を実装する。補助として、agent 自己申告に依存しない runtime 停滞検知による昇格提案 event（push 型安全網）を発行する。

## Why This Slice Exists Now

pre-routing（Layer A、v02-entry-pre-routing）で entry 判定が Direct になった run でも、実行中に複数モジュール横断・依存調査が必要・並列化したいと判明する経路が残る。現行 runtime には Worker から Orchestrator へ昇格する経路が存在せず、`finish` は `FinishArgs { result: String }` の即受理のみ、delegate は子 run 生成しかできない。v02-orchestrator-loop が昇格先の Orchestrator loop を提供した今、mid-run の Direct→Orchestrator 昇格（Layer B）を正式化する段階にある。LLM エージェントには構造的な継続バイアスがあり escalation の自己申告は滅多に発火しないため（interview Q2）、meta-op 呼出し任せにせず、runtime 側の停滞検知による昇格提案 event を安全網として併設する。

## Current Observed State

- meta-op は `crates/runtime/src/meta/mod.rs:34-58` の文字列 match で dispatch され、delegate/send/send_message/wait_reply/inbox/wait/cancel/list_agents/inspect_agent/skill_load/compact/finish が登録済み。構造化 handoff/memo 型は存在しない
- delegate は `{role, prompt, interactive, name, category, workspace_mode, load_skills}`（`meta/delegation.rs:10-38`）で子 run を生成するだけで、呼出元 run を terminal にして root run へ置き換える操作はない
- `finish` は result を即受理するだけで、workspace 譲渡や後続 run 起動の concept はない
- `RunConfig`（`crates/runtime/src/run.rs:57-73`）に handoff/memo フィールドはなく、親参照も持たない
- run phase は Pending/Running/Waiting/Done/Error（`crates/event-bus/src/event.rs:125-135`）を event bus へ emit するが、EscalationRequested や昇格提案 event は未定義
- v02-workspace-isolation が runtime 所有 git worktree（branch `evorch/task/<run-id>`、cleanup で worktree 削除・branch 保持）を提供済み

## Accepted Baseline You May Assume

- interview new-session.json Q3 確定: workspace は引き継ぐ（旧 run terminal 保証後に排他的に譲渡）。memo に `files_touched: Vec<PathBuf>` を含める。EscalationMemo スキーマは `EscalationMemo { source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state(dirty files/summary), escalation_reason, suggested_next }` で確定
- interview Q2 確定: LLM エージェントの継続バイアスにより escalation 自己申告は滅多に発火しない。補助として連続 edit 失敗・反復書き換え・tool call 閾値による runtime 停滞検知の昇格提案 event を持つ
- interview Q5 確定: EscalationRequested event（source_run_id、memo 概要、新 run_id）を event bus に発行し、v02-orchestrator-loop の observability（ToolStarted/Completed 同様の event bus）に乗せる
- ADR 0022: 親子限定ツリー addressing で sibling 間通信は orchestrator が中継。逆向き delegation edge を避けるため、新 run は旧 run の子ではなく source_run_id を保持する root run とする
- ADR 0001 / 0002: 固定 workflow ではなく invariant 下の動的 topology、role capability 境界の下で実装する
- dependencies: v02-entry-pre-routing（Layer A entry 判定）、v02-orchestrator-loop（昇格先 Orchestrator loop と event bus observability）

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/agents/`

Target part: Worker 直接起動 run が手に負えないと判断した際、構造化 EscalationMemo を記録して旧 run を terminal にし workspace を排他的に譲渡したうえで新規 Orchestrator root run を起動する escalation meta-op slice

## In Scope

- EscalationMemo 型（source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state, escalation_reason, suggested_next）の runtime 定義
- Worker run から呼べる専用 control-plane meta-op を新規 handler module 追加 + match arm 登録で `meta/mod.rs` dispatch へ実装
- memo 記録後に旧 run を terminal 化する ordering: 新規 tool call 停止、in-flight tool 完了/abort、pending send/wait_reply/inbox の close、phase の Done/Error 遷移
- terminal 保証後の workspace 排他譲渡（同時 writer を許さない二重変更不可の保証）
- source_run_id を保持する新規 Orchestrator root run の起動と、memo の初期 context 注入
- EscalationRequested event（source_run_id、memo 概要、新 run_id）の event bus 発行
- runtime 停滞検知（連続 edit 失敗 / 反復書き換え / tool call 数閾値）による昇格提案 event

## Out Of Scope

- pre-routing 判定そのもの（Layer A は v02-entry-pre-routing の責務）
- Orchestrator loop 本体の goal/gate/review/merge logic（v02-orchestrator-loop の責務）
- parked run の in-flight snapshot 厳密 revive（v0.3）
- 引き継いだ workspace 変更の自動 commit / merge。譲渡するのは排他所有権のみ
- 昇格提案 event を受けた自動強制昇格。提案は観測 event に留め、実昇格は memo 付き meta-op 経由のみ
- 固定 workflow DSL

## Standalone Child Issue Contract

Worker 直接起動 run の model が専用 meta-op を呼ぶと、runtime は構造化 `EscalationMemo`（source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state(dirty files/summary), escalation_reason, suggested_next）を freeze して記録し、旧 run を terminal 化する。terminal 化は新規 tool call の受付停止、in-flight tool の完了または abort、pending send/wait_reply/inbox の close、phase の Done/Error 遷移をこの順で完了させ、完了後にのみ workspace（v02-workspace-isolation の runtime 所有 worktree）を排他的に新 run へ譲渡する。新 run は旧 run の子ではなく ADR 0022 の tree semantics を守る Orchestrator root run として起動し、memo を初期 context に、source_run_id を保持する。handoff 時に EscalationRequested event（source_run_id、memo 概要、新 run_id）を event bus に発行する。さらに agent 自己申告に依存しない安全網として、連続 edit 失敗・反復書き換え・tool call 数閾値を観測する runtime 停滞検知が昇格提案 event を発行する。pre-routing 判定、Orchestrator loop 本体、workspace 変更の自動 merge、提案からの自動強制昇格は実装しない。PR は `main` を target にする。

## Acceptance Criteria

- EscalationMemo 型（source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state, escalation_reason, suggested_next）が runtime に定義される
- Worker run から呼べる専用 meta-op が meta dispatch に登録され、呼出時に memo を記録し旧 run を terminal 状態（新規 tool call 停止・実行中 tool 完了/abort・phase を Done/Error 遷移）にする
- 旧 run terminal 保証後に workspace が排他的に新 Orchestrator root run へ譲渡される（二重変更不可）
- 新 Orchestrator root run が memo を初期 context として起動し、source_run_id を保持する
- EscalationRequested event（source_run_id、memo 概要、新 run_id）を event bus に発行する
- runtime 停滞検知による昇格提案 event が発行され、agent 自己申告への依存でない安全網となる
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- EscalationMemo の serde round-trip と全必須フィールドの存在 test
- meta-op dispatch の arm 登録 test と invalid args の fail-closed 拒否 test（既存 parse pattern に倣う）
- terminal ordering test: 新規 tool call 停止 → in-flight 完了/abort → pending send/wait_reply/inbox close → phase Done/Error → 譲渡 の順序を検証し、譲渡が terminal 保証前に起こらないことを検証
- workspace 排他所有権 test: 譲渡後に旧 run からの書き込み経路が残らず、同時 writer が存在しないことを検証
- 新 run が root run として起動し（旧 run の子でないこと）、source_run_id と memo 初期 context を持つ test
- EscalationRequested event payload（source_run_id、memo 概要、新 run_id）と wire format round-trip test
- 停滞検知の table-driven test: 連続 edit 失敗 / 反復書き換え / tool call 閾値それぞれで提案 event が発火し、正常進行では発火しないこと
- workspace 全体の `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/orchestration/overview.md
- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/decisions/0001-no-fixed-workflow.md
- intents/evorch/decisions/0002-role-capability-boundaries.md
- intents/evorch/decisions/0022-parent-child-tree-addressing-and-nested-delegation.md
- intents/evorch/interviews/new-session.json
- crates/runtime/src/meta/mod.rs
- dependencies: `v02-entry-pre-routing`, `v02-orchestrator-loop`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `intents/evorch/features/orchestration/overview.md` primary（v0.2 確定節に 2026-09-03 付で Direct→Orchestrator 昇格が記載済み）。supporting は agent-runtime-kernel overview、ADR 0001、ADR 0022、interview new-session.json。新規 intent 不要
- ADR candidate: none（ADR 0022 の tree semantics の適用範囲内の実装）
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes。escalation meta-op、EscalationMemo スキーマ、workspace 排他譲渡の実装完了を `intents/evorch/features/orchestration/overview.md` の v0.2 確定節へ反映する

## Guide Reachability (G645)

Orchestrator role 向け surface（Direct→Orchestrator 昇格先の起動経路）を追加する。route: orchestration overview の v0.2 escalation surface → role Orchestrator → run lifecycle / workspace handoff。`no_role_facing_surface: false`。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

作業 branch は `evorch/task/<run-id>` 規約を用い、child PR はすべて `main` へ直接 open する。
