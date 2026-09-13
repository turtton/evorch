## Goal

Terminal pane のコンテンツ描画を検証マトリクスでカバーし、docs/gui-verification.md の
カバレッジ表に記載された Terminal pane の gap（layout-presence のみで内容描画テストなし）を
解消する。

## Why This Slice Exists Now

size x DPI ジオメトリ行列（tests/size_dpi_matrix）は 4 状態 x 4 サイズ x 3 DPI の 48 ケースで
稼働中（commit df4e66f）だが、docs/gui-verification.md のカバレッジ表で Terminal pane は
layout-presence のみで内容描画テストが gap と記載されている（commit e358b7e）。
検証 2 層方針（ADR 0015）のカバレッジを完了させる。

## Current Observed State

- ジオメトリ行列は 48 ケースで稼働中だが、Terminal pane の内容描画は検証されていない。
- docs/gui-verification.md のカバレッジ表に Terminal pane 行の gap が明記されている。

## Accepted Baseline You May Assume

- tests/size_dpi_matrix のジオメトリ行列基盤は実装済みで稼働中。
- 検証 2 層方針（ADR 0015）に従う。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/tests/size_dpi_matrix, crates/gui/tests, crates/gui/src/panes`

Target part: Terminal pane の検証カバレッジ

## In Scope

- Terminal pane の内容描画を検証するテストの追加
  （ジオメトリ行列への状態追加または headless kittest による必須要素検証）
- docs/gui-verification.md カバレッジ表の Terminal pane 行の gap 解消

## Out Of Scope

- 他 pane のカバレッジ拡張
- ジオメトリ行列基盤自体の再設計

## Standalone Child Issue Contract

evorch の Terminal pane について、内容描画を検証するテストを追加し（ジオメトリ行列への
状態追加または headless kittest による必須要素検証）、docs/gui-verification.md の
カバレッジ表で Terminal pane 行の gap を解消し、追加テストが cargo test --workspace
（GPU 不要レイヤー）または offscreen-gate の ignored sweep で緑になることを確認して
PR として提出する。

## Acceptance Criteria

- Terminal pane の内容描画を検証するテストを追加する（ジオメトリ行列への状態追加または headless kittest による必須要素検証）
- docs/gui-verification.md のカバレッジ表で Terminal pane 行の gap が解消される
- 追加テストが cargo test --workspace（GPU 不要レイヤー）または offscreen-gate の ignored sweep で緑になる

## Verification

- `cargo test --workspace`（GPU 不要レイヤー）全緑
- offscreen-gate の ignored sweep 緑（該当する場合）
- `git diff --check`

## Related Links

- ADR 0015: intents/evorch/decisions/0015-verification-two-layer.md
- 基盤 commit: df4e66f（ジオメトリ行列）、e358b7e（カバレッジ表 gap 記載）

## Knowledge Maintenance

- Intent placement: architecture overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: docs/gui-verification.md（カバレッジ表の Terminal pane 行を更新）
- Closeout writeback expected: no

## Guide Reachability (G645)

no_role_facing_surface: true（検証カバレッジの追加で role-facing surface の追加なし）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
