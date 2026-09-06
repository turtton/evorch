# v0.3: GUI デザイン修正（project 名ボックス過大・状態ドット配置/サイズ）

## Goal

operator 実使用で指摘された GUI workbench のデザイン問題 2 件を修正する。(1) project 名の表示ボックスが異常に大きい (2) 選択状態を表す青点/赤点がエリア右上に偏り、サイズが小さい。両方とも theme token 経由の実装で確定させる。

## Why This Slice Exists Now

v0.3 の GUI workbench は operator が実使用する対象であり、project セレクタ周りの視認性の悪さは日常操作の妨げになっている。v03-gui-design-refinement 等の大きな再設計の前に、指摘の明確な 2 点を小さく確定させる。

## Current Observed State

- project 名の表示ボックスが内容に対し異常に大きい（operator 指摘）
- 選択状態ドット（青/赤）がエリア右上に偏在し、サイズが小さい（operator 指摘）
- 該当箇所のサイズ/配置にハードコードが残っている（theme token 非経由）

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + egui / egui_dock（workspace Cargo.toml）
- v02 の確定: GUI workbench restructure（PR #66、project/thread 中心 3 領域レイアウト）
- theme token 基盤（crates/gui/src/theme/）と headless capture（crates/gui/src/headless.rs）は既存を流用

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/
- Part: project 名ボックスのサイズ修正 + 状態ドット（青/赤）の配置/サイズ修正（theme token 経由）

## In Scope

- project 名表示ボックスのサイジング修正
- 選択状態ドットの再配置とサイズ拡大
- theme token（crates/gui/src/theme/）への集約（ハードコード撤去）
- headless capture の before/after 取得

## Out Of Scope

- workbench レイアウト自体の再設計（pane 構成変更）
- 新規 widget / アニメーション導入
- 他 pane（terminal / tasks 等）のデザイン変更

## Standalone Child Issue Contract

本 PR は operator 指摘 2 件の修正と before/after による視覚検証を単独で示す。レイアウト再設計は後続 slice。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: ボックスサイズ修正 / ドット配置・サイズ修正 / theme token 実装 / before/after 添付 / 品質ゲート）。

## Verification

- focused tests: 修正箇所の描画・サイジングに関する既存テストが pass
- headless capture の before/after 画像を PR に添付
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ状態ドット配置ルールとサイジング規約を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の workbench 視覚デザイン
- role: Operator
- target_surface: project セレクタの表示と状態ドット

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
