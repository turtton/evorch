# v0.3: event-bus MessageDelta/ReasoningDelta に run_id を付与し並行 run streaming の attribution を成立させる

## Goal

`crates/event-bus/src/event.rs` の `MessageDelta` / `ReasoningDelta` イベントに `run_id` field を追加し、並行 run の streaming イベントが run 別に attribution されるようにする。GUI TranscriptRegistry の決定的配送、otel mapping、storage schema への追従を含む。

本 PR は Closes #78 となる。

## Why This Slice Exists Now

v02-gui-workbench-restructure（PR #66）で複数 transcript pane を並行表示する際、MessageDelta / ReasoningDelta に run_id がなく、並行 run の streaming が単一 pane に混線する制約を検出済み（gui-workbench overview 実装確定節に記録）。v0.3 の GUI workbench 完成を前提に、event schema の相関 field 欠落を解消する必要がある。

## Current Observed State

- `crates/event-bus/src/event.rs`: `MessageEvent::MessageDelta` / `ReasoningDelta` は `delta` のみで run_id を持たない
- 他のイベント variant（AgentRunStarted 等）には run_id / parent_run_id 相関 field が存在する
- GUI TranscriptRegistry はイベント配送を行うが、MessageDelta / ReasoningDelta の run 別振り分けができない構造
- otel mapping（metrics / span exporter）と storage schema がイベント定義に追従する必要がある

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + Tokio async runtime（workspace Cargo.toml）
- v02 の確定: 型付き event bus（v01）、GUI workbench restructure（PR #66）、otel metrics / span exporter（v02）
- event schema の後方互換方針（additive 拡張 or versioning）は intents に従う

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/event-bus/ crates/runtime/ crates/gui/
- Part: MessageDelta / ReasoningDelta の run_id field 追加 + 並行 run streaming の attribution 成立（GUI 配送、otel mapping、storage schema 追従）

## In Scope

- `crates/event-bus/src/event.rs` の MessageDelta / ReasoningDelta に run_id field 追加（schema additive、replay 互換）
- 並行 run の streaming が run_id で attribution される runtime 経路の確保
- GUI TranscriptRegistry の決定的配送で run 別 streaming を分離
- otel mapping / storage schema の追従

## Out Of Scope

- event bus そのものの再設計（既存 variant の意味論は維持）
- 新規イベント variant の追加
- GUI の並行 pane UI デザイン変更（本 slice は attribution 基盤の提供まで）

## Standalone Child Issue Contract

本 PR は並行 run の streaming イベントが run_id で attribution されることを単独で示す。GUI の並行 pane 表示最適化は後続 slice。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: run_id 追加（後方互換）/ 並行 run attribution / 既存表示回帰なし / otel・storage 追従 / 品質ゲート）。

## Verification

- focused tests: 並行 run streaming の run_id attribution test、serialization / replay 互換 test
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/features/gui-workbench/overview.md
- 関連 issue: #78（本 PR が Closes #78）
- 背景: PR #66（v02-gui-workbench-restructure）実装確定節: intents/evorch/features/gui-workbench/overview.md

## Knowledge Maintenance

- Intent placement: agent-runtime-kernel overview へ run_id attribution の確定方式（additive / versioning の採用決定）を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（agent-runtime-kernel overview）

## Guide Reachability (G645)

- no_role_facing_surface: true（event schema の相関 field 追加で新しい role-facing surface はない）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
