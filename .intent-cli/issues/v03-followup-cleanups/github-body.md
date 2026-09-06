# v0.3: follow-up 整理（ReasoningDelta production emit / frame.rs legacy mirror 撤去 / agents グリッド列幅）

## Goal

v0.3 進行中に蓄積した follow-up 3 件を整理する。(a) ReasoningDelta の production emit 経路新設（runtime が Reasoning コンテンツを bus event 化）(b) crates/gui/src/app/frame.rs の run_id: None legacy mirror 撤去（PR #86 で縮小済みの残り）(c) agents グリッド列幅の自動フィット（horizontal scroll の代替）。

## Why This Slice Exists Now

v03-event-runid-attribution で event schema の run_id 基盤が整うに伴い、Reasoning 表示の production 化と legacy mirror の撤去が可能になった。agents グリッドの列幅問題も operator 実使用での指摘に基づく。v0.3 完了前に整理して仕上げる。

## Current Observed State

- crates/runtime/src/agent_loop.rs: ContentBlock::Reasoning を処理する経路はあるが、ReasoningDelta として bus emit される production 経路は未完成
- crates/gui/src/app/frame.rs:51-52: MessageDelta / ReasoningDelta の run_id: None legacy match arm が残っている（PR #86 で縮小済みの残り）
- agents グリッドは列幅が固定で horizontal scroll が発生する

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + egui（workspace Cargo.toml）
- v0.3 の確定: v03-event-runid-attribution（run_id attribution 基盤）、v03-gui-workbench-restructure（PR #66）
- frame.rs legacy mirror は run_id attribution 完成後の撤去が前提

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/runtime/ crates/gui/
- Part: (a) ReasoningDelta production emit (b) frame.rs legacy mirror 撤去 (c) agents グリッド列幅自動フィット

## In Scope

- runtime の Reasoning → ReasoningDelta bus emit 経路
- crates/gui/src/app/frame.rs の run_id: None legacy arm 撤去
- agents グリッド列幅の自動フィット実装

## Out Of Scope

- Reasoning 表示のデザイン変更（既存 TranscriptEntry::Reasoning 表示の維持）
- frame.rs 自体の全面的な書き換え（legacy mirror 撤去のみ）
- グリッド以外の pane レイアウト変更

## Standalone Child Issue Contract

本 PR は 3 件の follow-up を整理し、各項目の検証テストと回帰なしを単独で示す。新規機能は含まない。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: ReasoningDelta emit / legacy mirror 撤去 / グリッド列幅 / 回帰なし / 品質ゲート）。

## Verification

- focused tests: ReasoningDelta emit test、frame.rs legacy 撤去後の表示維持 test、グリッド列幅フィット test
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/features/gui-workbench/overview.md
- 関連: v03-event-runid-attribution（run_id 基盤）、PR #86（frame.rs 縮小）

## Knowledge Maintenance

- Intent placement: agent-runtime-kernel overview へ ReasoningDelta emit と legacy 撤去の確定を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（agent-runtime-kernel overview）

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の agents グリッド
- role: Operator
- target_surface: agents グリッドの列幅自動フィット

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
