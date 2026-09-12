## Goal

send ツールを統合 interface として拡張し、終了・切断・プロセス再起動でメモリ上に存在しない
run への配送を storage projection からの AgentContext 再構成で透過的に復元する
（interface 統合・機構分割）。加えて compaction を跨いでも失われない run スコープの
durable 作業台帳（append-only ledger）を実装する。

## Why This Slice Exists Now

2026-09-12 の 11 機能 wave（ADR 0025 参照）で実証: サブエージェント session の再開が
3 度の stream 断からの復旧を可能にし、逆に session 失効は文脈の全損を招いた。
長時間の agent run の信頼性は「再開可能性」に依存する。
現状 evorch の send は生存中 run 専用で、Done/Error への配送は
runtime.rs:1060-1065 が RunTerminated で明示拒否する。終了した run の会話を
続ける経路が存在しない。

## Current Observed State

- LoopState（crates/runtime/src/agent_loop.rs）は run 専有の AgentContext を持ち、
  run 終了後に再開する API はない。
- SessionSnapshot（crates/storage/src/projection.rs）は event-sourced projection として
  message/reasoning 差分を読み戻せるが、AgentContext の再構成には使われていない。
- compaction（v02 設計）は model-visible window を狭めるが、作業上の決定・証拠を
  保持する専用 surface はない。

## Accepted Baseline You May Assume

- SessionSnapshot / storage writer / event-bus は実装済み（ADR 0017/0018）。
- v02-context-compaction の設計（raw history 非破壊）に従う。台帳は compaction 対象外。
- v07-d の memory ledger 基盤は共有可能だが、目的は別（in-run 作業記録）。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/ crates/storage/ crates/gui/`

Target part: session continuation API（task_id 相当で run を再開）+ run 台帳（tool 経由追記、
SQLite 永続化、compaction 非対象）+ GUI での続行表示

## In Scope

- send の統合 interface 化: 生存中 run へは現行どおり mailbox 配送（Wake/Steering/Aside）、
  終了・メモリ不在の run へは storage projection から AgentContext を再構成して起動し、
  メッセージを新しい turn として注入する
- Done/Error の再開 semantics は「新しい turn として注入して起動」に統一。
  Error 時の部分状態（open tool calls 等）は投影の復元責務とする
- 復元は重い処理のため内部機構は分割し、復元発生時に event を発行して観測可能にする
  （silent な復活はしない）
- 親子関係の認可境界（MessageDenied）は復元経路でも維持する
- agent が tool 経由で追記できる run スコープの append-only 台帳
- 台帳の SQLite 永続化と run 再開・GUI 表示からの参照
- 復元不能時の型付きエラー（silent な新規 session への fallback はしない）

## Out Of Scope

- task 境界を跨ぐ知識蓄積（v07-d の memory backend の領域）
- compaction engine 自体（v02-context-compaction の領域）
- マルチプロセス間の run 移譲（ADR 0024 の handoff の領域）
- 生存中 run への既存 send semantics（Wake/Steering/Aside/親子認可）の変更 —
  回帰テストで不変を固定する

## Standalone Child Issue Contract

evorch の runtime の send（AgentMessage）を統合 interface として拡張し、「終了・中断した
run へ send されたメッセージを、storage projection から復元した完全な会話コンテキスト付きで
新しい turn として処理する」機能と、「run スコープの append-only 作業台帳に agent が tool 経由で
追記でき、内容が compaction 後も保持され SQLite に永続化される」機能を追加し、
生存中 run への既存 semantics が不変であることの回帰テストを含むテスト群を添えて
PR として提出する。復元発生時は event を発行し、復元不能時は型付きエラーを返す。

## Acceptance Criteria

- 中断 run への send でコンテキストが一致して再開される（回帰テスト）
- 生存中 run への send の既存 semantics が不変（回帰テスト）
- 復元発生が event として観測できる
- 台帳追記が compaction 前後で保持される（回帰テスト）
- 台帳が GUI から読める
- 復元不能が型付きエラーで明示される

## Verification

- `cargo test -p runtime -p storage -p gui` 全緑
- 新規回帰テスト: 再開コンテキスト一致、台帳の compaction 跨ぎ保持
- `git diff --check`

## Related Links

- ADR 0025: intents/evorch/decisions/0025-agent-experience-evaluation.md
- ADR 0017 / 0018: event bus / SQLite storage schema

## Knowledge Maintenance

- Intent placement: orchestration overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

- guide surface: `guide workflow task implementation-loop`（role: implementation）
- target surface: runtime session continuation API + run ledger tool + GUI 続行表示

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
