# ADR 0038 ③: Codex / OpenAI の画像生成 capability と generate_image ツール（生成・編集）

## Goal

Codex サブスクリプション（ChatGPT ログイン）と OpenAI API キーのどちらでも画像を生成・編集できる provider の capability を加え、`generate_image` ツールで visual Worker と会話の root Worker が素材を作れるようにする。

## Why This Slice Exists Now

ADR 0038 で、デザイン相談の素材を agent が生成し、HTML モックに組み込む方針に合意した。v15-a（HTML の相対パス画像の埋め込み）と v15-b（モデルの種別と画像生成のチェーン）の上で、生成そのものをつなぐ。

## Current Observed State

- provider に画像生成の経路はない。Codex は Web 検索と要約（compaction）だけを任意 capability として持つ。
- Codex 本家は `{codex}/images/generations` と `/images/edits` を直接呼び、モデルを `gpt-image-2` に固定している。OpenAI 公開 API も同じ形の JSON を受け付ける。
- tool 結果で model に画像を返せる（`ToolResultContent::Image`、`inspect_artifact` の前例）。

## Accepted Baseline You May Assume

- 対応する provider は `openai-codex` と `openai` だけ。
- 付与の条件は `render_artifact` と同じ（visual Worker、会話の root Worker）で、`agents.roles.image_generator` が空なら付与しない。
- 保存先と参照画像はワークスペースかその thread の一時領域の中。
- 参照画像を OpenAI に送ることは、tool の説明と結果に明記するだけで、承認は挟まない（利用者合意）。
- 内容の拒否は次の候補へ進まない。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/providers/` `crates/event-bus/` `crates/storage/` `crates/runtime/` `crates/tools/` `crates/config/assets/presets/` `crates/gui/` `docs/` `intents/`
- Target part: provider の画像 capability（Codex / OpenAI）、`RequestPurpose::ImageGeneration`、画像生成のチェーンの解決、`generate_image` meta op と policy、保存・縮小版・プレビュー、overlay・docs。

## In Scope

- `ImageGeneration` trait と `ProviderClient::image_generation()`、共通の要求・応答・エラーの型。
- Codex（ChatGPT のセッションと既存ヘッダー）と OpenAI（API キー）の生成・編集の実装、応答の上限と検証、usage の記録、エラーの分類。
- `RequestPurpose::ImageGeneration` と網羅 match の追従。
- `RoutedModel` の画像生成用の解決（フォールバックとクールダウン）。
- `generate_image`: 付与の条件、引数、パスの検査、参照画像の検査と縮小、保存、縮小版、プレビュー画像と結果、取り消し。
- tool 説明、`category-visual` / `category-conversation` overlay、`docs/image-generation.md` ほか。

## Out Of Scope

`openai-compatible` などその他の provider、マスク指定の部分編集、複数枚の同時生成、生成画像の自動評価、tool 単位の利用上限、部分画像のストリーム表示、`/v1/models` の絞り込み。

## Acceptance Criteria

1. `ProviderClient::image_generation()` を加え、`openai-codex` と `openai` だけが対応する。要求・応答は provider 共通の型。
2. Codex は ChatGPT のセッションと既存ヘッダーで `{codex}/images/generations` / `/images/edits` に JSON を送り、`data[].b64_json` を復号する。
3. OpenAI は API キーで `{base_url}/images/generations` / `/images/edits` に JSON を送り、`output_format: png` を指定する。
4. 応答本文 48 MiB・全体 5 分の上限、空の `data`・不正な base64・png でない応答の失敗、`RequestPurpose::ImageGeneration` での usage の 1 回だけの記録。
5. 認証・利用上限・レート制限・一時的な失敗・endpoint の非対応はフォールバックの対象、内容の拒否と不正な要求は次の候補へ進まず model に返す。
6. `generate_image` は visual Worker と会話の root Worker で、run の開始時に `image_generator` のチェーンが空でない場合だけ公開される。policy と dispatch の両方で確かめる。
7. チェーンを router で解決し、候補が `image_generation` で画像 capability を持つことを確かめてから送る。
8. 引数は prompt、output_path（.png）、reference_image_paths（1〜5）、transparent_background、variant、size、quality。
9. 保存先と参照画像はワークスペースか同じ thread の一時領域の中。書き込みは `write` と同じ境界と ownership の検査を通し、rename で行う。
10. 参照画像は形式・マジックバイト・8 MiB を確かめ、長辺 2048 px を超えれば縮小して送る。
11. variant で縮小版（長辺 64〜2048、jpeg / png）を保存する。jpeg と透過の組み合わせは拒否する。
12. 結果はパス・寸法・profile・モデル・参照画像の送信の有無と、長辺 1024 px 以下のプレビュー画像。参照画像を送った場合は OpenAI への送信を明記する。
13. tool 説明に送り先、参照画像の送信、利用枠の消費、採否の判断を agent に任せることを書く。
14. overlay に、素材を生成して縮小版を HTML から相対パスで参照し取り込む手順と、生成画像を UI モックの代わりにしないことを書く。
15. 取り消しで要求を中断し、ファイルを書かない。
16. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: providers の wiremock テスト（要求の形、応答の検証、上限、usage、エラーの分類）、runtime の付与条件・パス・参照画像・縮小版・フォールバック・取り消し・結果。
2. Integration: scripted model と mock endpoint で、生成 → HTML への組み込み → `render_artifact` → `present` の流れ（Orchestrator の会話と root Worker の会話）。Codex の 404 から OpenAI へのフォールバック。
3. Build / lint: `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`scripts/check-cache-contracts.sh`、`git diff --check`。
4. 手動: 実アカウントの Codex サブスクリプションと OpenAI API キーで生成と編集が成功すること。未実施の場合は PR に未検証と明記する。

## Related Links

- `intents/evorch/decisions/0038-image-generation.md`
- `intents/evorch/decisions/0029-conversation-artifacts.md`
- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- Codex 本家: `codex-rs/codex-api/src/endpoint/images.rs`、`codex-rs/ext/image-generation/src/tool.rs`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
