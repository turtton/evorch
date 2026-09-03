# v02-direct-escalation-handoff Implementation Packet

## Goal

Worker 直接起動（Direct）run の model が専用 control-plane meta-op を呼ぶと、runtime が構造化 `EscalationMemo`（source_run_id, original_request, findings, files_touched: Vec<PathBuf>, blockers, workspace_state(dirty files/summary), escalation_reason, suggested_next）を記録し、旧 run を terminal 化したうえで workspace を排他的に譲渡して新規 Orchestrator root run を起動する。handoff は EscalationRequested event で観測可能にし、補助として runtime 停滞検知による昇格提案 event（push 型安全網）を発行する。

## Why

現行 runtime の meta-op は `crates/runtime/src/meta/mod.rs:34-58` の文字列 match で dispatch され、構造化 handoff/memo 型は存在しない。`delegate`（`meta/delegation.rs:10-38`）は子 run を作るだけで呼出元 run を terminal にする操作はなく、`finish` は `FinishArgs { result }` の即受理のみ。interview Q3 で workspace 引き継ぎ・`files_touched: Vec<PathBuf>` 同梱・EscalationMemo スキーマが確定し、Q2 で escalation 自己申告が滅多に発火しない（継続バイアス）ことが記録された。ADR 0022 の親子限定ツリー addressing の下では escalation は逆向き delegation edge を作れないため、旧 run を terminal にして新規 root run を起動する形が必要になる。

## Scope

- `EscalationMemo` 型を runtime crate に定義する。フィールドは確定スキーマどおり: `source_run_id`（`crate::run::RunId`）、`original_request`、`findings`、`files_touched: Vec<PathBuf>`、`blockers`、`workspace_state`（dirty files 一覧 + summary）、`escalation_reason`、`suggested_next`。event/transcript へ記録できるよう Serialize を備える
- `crates/runtime/src/meta/escalation.rs` を新規 handler module として追加し（`delegation.rs` / `messaging.rs` と同じ pattern）、`meta/mod.rs:6-9` の module 宣言へ `mod escalation;` を足し、`meta/mod.rs:34-58` の dispatch match に新 arm を登録する。args は既存の `parse::<T>` fail-closed pattern で検証し、memo の各フィールドを model から受け取る
- handler の ordering を terminal-before-transfer で固定する: (1) memo を freeze して記録、(2) 旧 run の新規 tool call 受付を停止、(3) in-flight tool の完了または abort を待つ、(4) pending の send/wait_reply/inbox を close する、(5) phase を Done または Error（`crates/event-bus/src/event.rs:125-135` の `AgentRunPhase`）へ遷移させる。workspace 譲渡と新 run 起動はこの全工程の完了後にのみ行う
- workspace は v02-workspace-isolation の runtime 所有 worktree 機構（`crates/runtime/src/workspace.rs` の `WorktreeManager` / `OwnedWorktree`、branch `evorch/task/<run-id>`）を使い、旧 run の所有権を新 run へ移す。排他性は所有権の移動で表現し、terminal 保証前の譲渡と譲渡後の旧 run からの書き込み経路を構造的に排除する。変更内容の自動 commit/merge は行わない
- 新 run は Orchestrator role の root run として起動する。`delegate_background_as_child` のような child 生成経路は使わず（`RunConfig`（`run.rs:57-73`）は親参照を持たない）、memo を初期 context に注入し `source_run_id` を保持させる。ADR 0022 の tree semantics を破る逆向き delegation edge を作らない
- `EscalationRequested` event を `crates/event-bus` の `EventKind`（ToolStarted 等と同じ durable event bus）へ追加する。payload は source_run_id、memo 概要、新 run_id。wire format の serde round-trip test を既存 event の pattern に倣って追加する
- push 型安全網として停滞検知を runtime に追加する。連続 edit 失敗回数、同一ファイルの反復書き換え、run あたり tool call 数の閾値を観測し、超過時に昇格提案 event を発行する。提案は観測 event に留め、自動強制昇格はしない。閾値は調整可能にする

## Out of scope

- pre-routing 判定（Layer A）そのもの。entry 側の Direct/Coordinated 判定は v02-entry-pre-routing の責務
- Orchestrator loop 本体の goal/gate/review/merge logic。v02-orchestrator-loop の責務
- parked run の in-flight tool/model snapshot を復元する厳密 revive（v0.3）
- 引き継いだ workspace 変更の自動 commit / merge / branch 統合
- 昇格提案 event からの自動強制昇格。実昇格は memo 付き meta-op 呼出し経由のみ
- 固定 workflow DSL。escalation は invariant 下の動的 topology の一部として実装する

## Verification

- memo 型単体: EscalationMemo の serde round-trip、必須フィールド存在、`files_touched: Vec<PathBuf>` の型検証
- meta-op dispatch: 新 arm が `meta/mod.rs` の match 経由で呼ばれること、invalid args がエラーで fail-closed に拒否されること（runtime 不在時の `error("runtime is unavailable")` 経路を含む既存 pattern に整合）
- terminal ordering: tool call 停止 → in-flight 完了/abort → pending send/wait_reply/inbox close → phase Done/Error → 譲渡・新 run 起動 の順序を守る test。in-flight tool 実行中の meta-op 呼出しで abort/待機が起きること、譲渡が terminal 保証完了前に発生しないことを検証
- workspace 排他所有権: 譲渡後に旧 run の tool が workspace へ書き込めないこと、同時 writer が存在しないこと、`OwnedWorktree` の所有権で二重変更不可が表現されていること
- root run 起動: 新 run が旧 run の子でないこと（親子 addressing に現れない）、`source_run_id` を保持し、memo が初期 context に含まれる test
- event: EscalationRequested の payload（source_run_id、memo 概要、新 run_id）と serde round-trip / wire format 互換 test
- 停滞検知: 連続 edit 失敗 / 反復書き換え / tool call 閾値それぞれで昇格提案 event が発火し、正常進行の run では発火しない table-driven test。提案が自動昇格へ繋がらないこと
- workspace 全体の `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: primary は `intents/evorch/features/orchestration/overview.md`（v0.2 確定節に 2026-09-03 付で Direct→Orchestrator 昇格が記載済み）。supporting は `features/agent-runtime-kernel/overview.md`、ADR 0001、ADR 0022、interview new-session.json。新規 intent node は不要
- ADR candidate: decline。親子限定ツリー addressing 下での terminal-then-root-run handoff は ADR 0022 の適用範囲内で、新規決定ではない
- Diagram candidate: decline
- Docs update: decline
- Closeout learning: escalation meta-op・EscalationMemo スキーマ・workspace 排他譲渡の実装完了を `intents/evorch/features/orchestration/overview.md` の v0.2 確定節へ書き戻す。`write_back_required: true`

- Guide reachability (G645): Orchestrator role 向け surface を追加する。route は orchestration overview の v0.2 escalation surface（Direct→Orchestrator 昇格）→ role Orchestrator → target surface は run lifecycle / workspace handoff。`no_role_facing_surface: false`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
