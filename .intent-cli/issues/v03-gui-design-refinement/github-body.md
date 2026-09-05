# v0.3: GUI デザインシステム整備（t3code 風ダークテーマへの洗練）

## Goal

egui 既定ダークテーマのまま並んだ現行 GUI を、t3code（b883fc0）を参照した洗練されたダークテーマのデザインシステムへ整備する。design token の単一モジュール化、3 領域レイアウトの視覚品質、空状態 placeholder、headless_capture の populated 対応を含む。

## Why This Slice Exists Now

v02-gui-workbench-restructure で 3 領域レイアウトの構造は landed したが、視覚設計は egui 既定のままで「プロトタイプ段階」の印象が強い（lead の headless capture 評価で確認）。実用上の利用に耐える視覚品質に引き上げる段階。

## Current Observed State

- Visuals / theme のカスタマイズ参照ゼロ（egui 既定ダークそのまま）
- 右 Agents 表の列クリッピング、タブ名と本文見出しの重複、極端な上下空白
- アクセントカラー・選択/hover 状態の表現なし、空状態 placeholder なし
- headless_capture は空状態 1 フレームのみ

## Accepted Baseline You May Assume

- v02-gui-workbench-restructure merged（3 領域レイアウト、PanelKind、schema v2、HeadlessWorkbench/egui_kittest）
- egui/eframe/egui_dock の技術選定（ADR 0007）は変更しない

## Target Repo / Path / Part

- Target repo: turtton/evorch
- Target paths: crates/gui/
- Target part: design token / theme 単一モジュール、3 領域視覚品質、空状態、capture 証跡

## In Scope

- theme/design token モジュール新設と全 surface 適用（ダークテーマ維持）
- sidebar 階層表示・選択ハイライト・状態 indicator
- conversation の視覚的 focal point 化（message card / composer）
- 右 tabs の active/hover/通知状態アクセント
- 空状態 placeholder + CTA
- 余白スケール統一（4px 基準 grid 等）
- headless_capture の populated（demo 相当）state 出力拡張 + before/after スクリーンショット

## Out Of Scope

- 機能追加（新規 pane / surface / 操作）、schema の非互換変更
- ライトテーマ
- t3code の pixel 完全コピー（参照はデザイン言語レベル）

## Standalone Child Issue Contract

本 PR は GUI の視覚品質を「egui 既定」から「t3code を参照した洗練ダークテーマ」へ引き上げる。構造（3 領域レイアウト・PanelKind・schema）は変えず、theme token の単一モジュール化と全 surface 適用で達成する。検証は headless capture の before/after スクリーンショットと既存 kittest 回帰で行う。

## Acceptance Criteria

packet の acceptance_criteria が権威（8 件）。要点: theme 単一モジュール / 3 領域の視覚検証（capture）/ 空状態 placeholder / タブ・見出し重複解消 + 余白一貫 / capture populated 拡張 + before/after 添付 / t3code 参照の適合説明 / 回帰維持 / 品質ゲート。

## Verification

- `cargo test --workspace` / clippy / fmt / `git diff --check` 全 pass
- headless_capture で populated state のスクリーンショット出力、PR に before/after 添付
- 既存 headless/egui_kittest 回帰 pass

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/decisions/0007-gui-framework-egui-first.md
- intents/evorch/interviews/grill-v02-loop-foundation.json
- デザイン prior art: t3code b883fc0
- dependency: `v02-gui-workbench-restructure`（#65、merged）

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ theme モジュール・デザイン言語確定点・capture 拡張を反映（closeout writeback）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の workbench surface
- role: Operator
- target_surface: GUI 3 領域レイアウトの視覚状態

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
