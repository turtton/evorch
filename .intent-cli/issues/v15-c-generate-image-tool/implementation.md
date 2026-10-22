# v15-c-generate-image-tool Implementation Packet

## Goal

Codex サブスクリプション（ChatGPT ログイン）と OpenAI API キーのどちらでも画像を生成・編集できる provider の capability を加え、`generate_image` ツールで visual Worker と会話の root Worker が使えるようにする。ADR 0038 の「provider の capability として実装する」「`generate_image` ツール」の節を実装する。

## Why

デザイン相談で、挿絵・アイコン・背景などの素材を agent が自分で用意し、HTML モックに組み込めるようにする。v15-a で HTML の相対パス画像が取り込み時に埋め込まれ、v15-b で画像生成のチェーンとモデルの種別が入るので、この slice で生成そのものをつなぐ。生成した画像を採用するか参考にとどめるかは agent が判断するため、tool 結果でプレビュー画像を model に返す。

## Scope

### providers

- **共通の型:** `ImageGenerationRequest { model, prompt, reference_images: Vec<InlineImage>, background: Opaque | Transparent, size: Option<String>, quality: Option<String>, observation, usage_sink }`、`ImageGenerationResponse { png: Vec<u8>, width, height, background, usage: Usage, request_id: Option<String> }`、`ImageGenerationError { Rejected(String) /* 内容の拒否・不正な要求 */, Provider(ProviderError) }`。`reference_images` が空なら生成、あれば編集の endpoint を使う。
- **trait:** `ImageGeneration`（`provider_name()`、`generate(auth, request)`）と `ProviderClient::image_generation() -> Option<&dyn ImageGeneration>`（既定は `None`）。
- **Codex:** `CodexClient` に実装する。`post_response` のヘッダー付与を共有の関数に切り出し、`Accept: application/json` で JSON を送る。endpoint は `resolve_endpoint` と同じ規則で `…/codex/images/generations` / `…/codex/images/edits` を作る。本文は `{prompt, model, background, quality: quality.unwrap_or("auto"), size: size.unwrap_or("auto")}`、編集は `images: [{image_url}]` を加える。`x-codex-imagegen-request-id` を `request_id` に入れる。
- **OpenAI:** `OpenAiClient` に実装する。`{base_url}/images/generations` / `/images/edits` に API キーで JSON を送る。本文は Codex と同じに `output_format: "png"` を加え、size / quality は指定されたときだけ送る。
- **応答の扱い:** 本文はストリームで読み、48 MiB を超えたら失敗にする。全体の時間は 5 分まで。`data` が空、base64 が不正、復号した内容が png でない場合は失敗。`usage`（input_tokens / output_tokens）を `Usage` に変換し、`UsageEmitter` と `usage_sink` で 1 回だけ記録する。`AttemptObserver` は `RequestPurpose::ImageGeneration`（event-bus に追加。fencing・storage の projection・GUI の使用量表示の網羅 match にも加える）。
- **エラーの分類:** 401 / 403 は認証、429 はレート制限（Retry-After を尊重）、利用上限は既存の quota の分類、5xx と通信の失敗は一時的な失敗、404 / 405 は endpoint の非対応として、どれも `Provider` にする（チェーンのフォールバックの対象）。400 の安全性の判定（`moderation_blocked` など）と不正な要求は `Rejected` にする。エラー本文は既存の `sanitize` と同じく秘密情報を伏せて短くする。
- **固定の画像モデル一覧:** v15-b で置いた一覧（Codex の `gpt-image-2`）を Codex の既定として使う。

### runtime

- **meta op:** `GenerateImage => "generate_image"` を `define_meta_ops!` に登録する。
- **付与の条件:** `ExecutionPolicy::for_run_config` で `may_render` が真で、かつ run の開始時の設定スナップショットで `agents.roles.image_generator` のチェーンが空でない場合だけ付与する。dispatch でも同じ条件を確かめる。run の途中の設定の再読み込みでツール定義を変えない。
- **解決:** `RoutedModel` に画像生成用の解決を加える（`resolve_summary_invocation` と同じく router でチェーンを解決し、クールダウンと失敗の記録を共有する）。解決した候補の種別が `image_generation` で、その profile の client が `image_generation()` を持つことを確かめる。`Provider` のエラーはチェーンの次の候補へ、`Rejected` は model へそのまま返す。
- **パスの検査:** `output_path`（`.png` 必須）と `reference_image_paths` は、ワークスペースかその run の thread の一時領域の中（canonicalize 後。存在しない output_path は親ディレクトリで判定）。v15-a の境界の判定を共有する。書き込みは `write` と同じ境界と ownership の mutation 検査（`validate_artifact_mutation` と同じ permit）を通し、同じディレクトリの一時ファイルから rename する。親ディレクトリは作らない（無ければエラー）。
- **参照画像:** 拡張子とマジックバイトを確かめ、8 MiB 以下。長辺 2048 px を超えるものは縮小して png の data URL にする。1〜5 件。
- **縮小版:** `variant: {max_side: 64..=2048, format: "jpeg" | "png"}`。jpeg は品質 85、`transparent_background` と jpeg の組み合わせは拒否。保存先は `<dir>/<stem>.<max_side>.<jpg|png>`。
- **プレビューと結果:** 長辺 1024 px 以下に縮小した画像を `ToolResultContent::Image` で返し、テキストに `{path, variant_path?, width, height, profile, model, reference_images_sent, note}` を返す。参照画像を送った場合は note に「reference images were sent to OpenAI」と書く。
- **取り消し:** run の取り消しで要求を中断し、ファイルを書かない（書き込みは応答の検証の後に行う）。
- **画像処理:** tools crate の `image` 依存を使う関数（縮小・形式の確認・エンコード）を `tools` に置くか、runtime に `image` を加える。CPU を使う処理は `spawn_blocking` で行う。

### prompt / docs

- **tool 説明:** 送り先が OpenAI であること、参照画像が送信されること、画像は利用枠を通常より多く消費すること、size と quality は Codex で無視されうること、生成画像を採用するか参考にとどめるかは自分で判断してよいこと。
- **overlay（category-visual / category-conversation）:** 素材が必要なときは `generate_image` で作り、縮小版を HTML から相対パスで参照して `render_artifact` で取り込む。UI 本体は HTML で作り、生成画像を UI モックの代わりにしない。シリーズの素材は前の画像を参照画像にして画風をそろえる。参考の素材は `$TMPDIR` に置く。
- **Docs:** `docs/image-generation.md`（新規。付与条件、設定例、引数、保存先、送信先、利用枠、未確認事項）。`docs/conversation-artifacts.md` と `docs/providers.md` から参照する。

### 採用・却下案と理由

- Responses の hosted `image_generation` tool を会話の要求に付ける案は採らない。会話用モデルを 1 回動かす必要があり遅く、Codex 本家もやめている。
- `codex exec` を子プロセスで呼ぶ案は採らない。遅く（第三者の報告で 1 回 4〜6 分）、Codex CLI の導入とログインが別に要る。
- 生成画像を artifact store に直接保存する案は採らない。HTML から相対パスで参照して組み込む流れ（v15-a）に乗らない。
- 内容の拒否で次の候補へ進む案は採らない。同じ判定を別の経路で回避することになるうえ、利用枠を無駄に使う。
- 参照画像の送信に利用者の承認を挟む案は採らない（利用者合意。tool の説明と結果に明記する）。

## Out of scope

`openai-compatible` などその他の provider。マスク指定の部分編集。1 回で複数枚（`n > 1`）。生成画像の自動評価（`MultimodalLooker` への委譲など）。tool 単位の利用上限や予算。生成の進行状況の表示（部分画像のストリーム）。`/v1/models` から非会話モデルを除く絞り込み。

## Verification

### Unit

- providers（wiremock）: Codex と OpenAI の生成・編集の要求の URL・ヘッダー・本文、応答の復号、48 MiB 超と時間切れ、空の `data`・不正な base64・png でない内容、usage の 1 回だけの記録、エラーの分類（401 / 403 / 429 / 404 / 5xx / 安全性の判定）、秘密情報を伏せたエラー本文。
- runtime: 付与の条件（visual Worker、会話の root Worker、子 run の conversation category、Orchestrator、チェーンが空のとき、run の途中の再読み込み）、policy と dispatch の一致、パスの検査（境界外・symlink・別 thread の一時領域・拡張子）、参照画像の検査と縮小、縮小版の保存と jpeg + 透過の拒否、`Provider` エラーでの次の候補へのフォールバックと `Rejected` での停止、取り消しでファイルが書かれないこと、tool 結果のプレビュー画像と note。

### Integration → build

- scripted model と mock の画像 endpoint で、visual Worker が素材を生成し、縮小版を参照する HTML を `render_artifact` で取り込み、root Orchestrator が `present` する流れ。root Worker の会話で同じ流れを 1 run で行う。
- Codex の profile が 404 を返すとき、同じ route の OpenAI の候補にフォールバックする。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 手動（実アカウント）

- Codex サブスクリプションで生成と編集が成功し、evorch の Codex の利用枠の表示（quota）か ChatGPT の使用量ダッシュボードで消費が見えること。
- OpenAI API キーで生成と編集が成功すること。
- 実施できなかった場合は PR に未検証と明記する。

### 未確認事項

- Codex の画像 endpoint が evorch の送信ヘッダーで受け付けられるか（本家のヘッダーとの差）。
- Codex 経路で `model` / `size` / `quality` が反映されるか。
- Codex 以外のクライアントから ChatGPT ログインで画像 endpoint を使うことの利用規約上の扱い。
