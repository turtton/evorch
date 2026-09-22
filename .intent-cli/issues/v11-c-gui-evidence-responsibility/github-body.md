# v11-c: GUI エビデンス収集の責務整理（headless_capture matrix script と #[ignore] 証跡テストの分界固定）

## Goal

GUI エビデンス生成系の責務を整理する。(a) `scripts/gui-evidence-matrix.sh` の theme × size 網羅証跡と `crates/gui/tests/` の `#[ignore]` 証跡テストの個別・操作系証跡の分界を固定し、(b) 3方向に分散している証跡シナリオ定義の単一 source 化を実施または見送り根拠を記録し、(c) 保存先規約違反の検出チェックを追加する。

## Why This Slice Exists Now

テスト資産の棚卸しで、GUI 証跡が2系統（matrix script と `#[ignore]` テスト）に分かれているが、分界は script 末尾のコメント1行のみで強制力がなく、状態定義は headless_capture の CLI flag / script 内 state 名 / 個別テスト本体に分散していることが判明した。重複・抜け・陳腐化が静かに進行しうるため、offscreen-gate CI の信頼性維持のために固定する。

## Current Observed State

- `scripts/gui-evidence-matrix.sh`: 2 theme × 6 state × 4 size + 2 state × 2 DPI = theme あたり 28 PNG を `cargo run -p gui --bin headless_capture` で生成し枚数 assert。規約「Ignored tests store their own evidence in subdirectories, outside this matrix」はコメントのみ
- `.github/workflows/ci.yml`: offscreen-gate job が `cargo test -p gui --tests -- --ignored --nocapture` → matrix script の順で実行し `gui-evidence` artifact を upload
- `crates/gui/tests/`: provider_settings/capture.rs, tabs.rs, model_metadata_capture.rs, pending_approvals_capture.rs, role_settings_headless/evidence.rs, legacy.rs, sandbox_composer_headless.rs 等の `#[ignore]` テストが個別に PNG 生成
- `crates/gui/src/bin/headless_capture.rs`: シナリオは `--demo` / `--error-thread` / `--pending-approvals` / `--open-theme-settings` / `--edit-profile` 等の CLI flag にハードコード
- matrix 側は枚数のみ検査で、個々の PNG の中身・最新性は無検証

## Accepted Baseline You May Assume

- `headless_capture` binary と offscreen-gate job は既存で稼働中（本 slice で新設はしない）
- `#[ignore]` 証跡テストのうち実 wgpu / GPU 依存のものは EVORCH_REQUIRE_ADAPTER=1 の CI 環境でのみ実行される現状維持

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ scripts/ .github/workflows/
- Part: エビデンス責務分界の固定、シナリオ定義源の整理、保存先規約の強制チェック

## In Scope

- matrix script と `#[ignore]` テストの責務分界の確定と明文化
- シナリオ定義の単一 source 化の実施または見送り判断（根拠を PR body に記録）
- matrix 出力直下への `#[ignore]` 側証跡の混入を検出するチェック
- 必要に応じた offscreen-gate job ステップ構成の調整

## Out Of Scope

- 証跡 PNG の画像差分比較・リグレッション検知基盤の構築
- `#[ignore]` 証跡テストの内容自体の追加・見直し（個別テストの中身は既存のまま）
- browser-e2e（chromiumoxide 系）の証跡運用

## Standalone Child Issue Contract

本 PR はエビデンス収集の責務整理を単独で示す。他 v11 系 slice（mock-openai 拡張系）への依存はない。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 分界の確定と overview 反映 / 単一 source 化の実施または見送り根拠記録 / 混入検出チェック / offscreen-gate 整合 / 品質ゲート）。

## Verification

- focused tests: `cargo test -p gui`（headless 系の既存テストとの非干渉確認）、script 変更時はローカルで `bash scripts/gui-evidence-matrix.sh` の少なくとも構文確認（offscreen-gate job での実走は CI 側）
- `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- offscreen-gate job（CI）での ignored 実行 + matrix 生成が成功
- Reviewer Gate（CI job と script を跨ぐ変更のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- 関連: v04-provider-settings-ux（provider settings 証跡の起点）、v08-a-browser-e2e-evidence（browser 証跡系。本 slice の対象外だが隣接）

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ責務分界と保存先規約を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- 開発 / review 工程の内部基盤整理のため role-facing の案内面は新設しない

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
