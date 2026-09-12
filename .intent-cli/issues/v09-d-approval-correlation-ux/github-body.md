## Goal

承認リクエストの GUI 表示に run:call:attempt の相関情報と対象 tool・引数の要約を明示し、
何の・どの試行の承認かをユーザーが判別できるようにする。

## Why This Slice Exists Now

2026-09-12 の Oracle レビューで runtime 側の confused-deputy 脆弱性を修正済み
（commit d5342ce: AskOnFailure/AskFirst の承認相関を run_id:call_id:一意 attempt 番号に束縛）。
本 unit はその GUI 側の仕上げ。並列 wave 実行（T9）で複数の承認要求が同時に上がり得るため、
どれを承認しているかの判別が実用上の安全性に直結する。

## Current Observed State

- 承認 event（crates/sandbox/src/approval.rs、ApprovalResolved 等）は相関 ID を持つ。
- GUI の承認プロンプトがこの相関 ID と対象 tool・引数の情報を表示するかは未整備。
- 複数の承認要求が同時に存在する場合の個別操作 surface は薄い。

## Accepted Baseline You May Assume

- runtime 側の承認相関（run_id:call_id:attempt）は修正済み（d5342ce）。
- 承認 event は相関 ID を持つ（crates/sandbox/src/approval.rs）。
- v09-b の通知モデル・run 一覧 surface は実装済み。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/ crates/sandbox/`

Target part: 承認プロンプト UI（相関 ID 明示・対象 tool/引数要約・複数要求の一覧個別操作）

## In Scope

- 承認プロンプトに対象 tool 名・引数要約・run/スレッド識別を表示
- 同時に複数の承認要求がある場合の一覧表示と個別承認/拒否
- 承認 UI の操作が対応する要求以外の call に効かないことの回帰テスト
- L2 headless テスト + L4 PNG 証跡（docs/gui-verification.md 準拠）

## Out Of Scope

- runtime 側の承認相関ロジック自体（d5342ce で修正済み）
- 承認ポリシー（AskOnFailure/AskFirst の条件）の変更
- 通知モデル全般の変更（v09-b の既存 surface を活用）

## Standalone Child Issue Contract

evorch の GUI 承認プロンプトに「run:call:attempt の相関情報と対象 tool・引数要約の表示」と
「複数承認要求の一覧・個別承認/拒否」を追加し、承認 UI 操作が対応する要求以外の call に
効かないことを回帰テストで固定し、L2 headless テスト + L4 PNG 証跡
（docs/gui-verification.md 準拠）を添えて PR として提出する。

## Acceptance Criteria

- 承認プロンプトに対象 tool 名・引数要約・run/スレッド識別が表示される
- 同時に複数の承認要求がある場合、一覧で個別に承認/拒否できる
- 承認 UI の操作が対応する要求以外の call に効かないことが回帰テストで固定される
- L2 headless テスト + L4 PNG 証跡（docs/gui-verification.md 準拠）

## Verification

- `cargo test -p gui -p sandbox` 全緑
- L2 headless テスト + L4 PNG 証跡（docs/gui-verification.md 準拠）
- `git diff --check`

## Related Links

- ADR 0008: intents/evorch/decisions/0008-threat-model-phased-adoption.md
- ADR 0025: intents/evorch/decisions/0025-agent-experience-evaluation.md
- intents/evorch/features/gui-workbench/overview.md

## Knowledge Maintenance

- Intent placement: gui-workbench overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

- guide surface: `guide workflow task implementation-loop`（role: implementation）
- target surface: 承認プロンプト UI

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
