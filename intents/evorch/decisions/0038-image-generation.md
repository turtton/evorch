# ADR 0038: 画像生成ツールと画像生成モデルの種別

## Status

Accepted（2026-10-22）。利用者との設計相談で方針に合意した。[ADR 0029](0029-conversation-artifacts.md) の `render_artifact` に、scratch 配下のファイルの受け付けと、HTML が参照する画像の埋め込みを加える。

## Context

デザイン相談で agent が取れる手段を増やしたい。HTML モックだけでは、挿絵・アイコン・背景・質感のような素材を用意できない。素材を画像生成で作り、HTML モックに組み込めれば、モックの表現力が上がる。生成した画像をそのまま採用するか、参考にとどめるかは agent の裁量とする。

調査で分かったこと（2026-10-22 時点）:

- Codex は ChatGPT サブスクリプション（Free 以外）で画像を生成・編集できる。消費は Codex の通常の利用枠に含まれ、画像のターンは通常の 3〜5 倍の速さで枠を減らす（developers.openai.com/codex/pricing）。
- Codex 本家は Responses の hosted `image_generation` tool をやめ、専用の endpoint を直接呼ぶ。`POST {codex}/images/generations` と `POST {codex}/images/edits` で、JSON の本文は `{prompt, model, size?, quality?, background?, n?}`、編集は `images: [{image_url: "data:..."}]` を加える。応答は非ストリームの JSON で `data[].b64_json` と `usage` を返す（`codex-rs/codex-api/src/endpoint/images.rs`）。
- 本家はモデルを `const IMAGE_MODEL = "gpt-image-2"` に固定し、品質とサイズは常に `auto` を送る。編集の参照画像は 5 枚まで（`codex-rs/ext/image-generation/src/tool.rs`）。モデル名を送っても無視される、という第三者の報告がある。
- OpenAI 公開 API は同じ形の `/v1/images/generations` と `/v1/images/edits`（JSON の `images[].image_url` に data URL）を持ち、`output_format` などを追加で受け付ける。画像モデルは `gpt-image-1` / `gpt-image-1-mini` / `gpt-image-1.5` / `gpt-image-2` / `gpt-image-2.5-*` / `chatgpt-image-latest` / `dall-e-*`（退役済み）で、名前は更新されていく。
- evorch の既存部品: Codex の OAuth セッションと送信ヘッダー（`providers/src/provider/codex/client.rs`）、provider の任意 capability（`HostedWebSearch`、`Compactor`）、agent として起動しないロールのチェーン（`agents.roles.compactor`、[ADR 0030](0030-role-model-chains.md)）、画像を返せる tool 結果（`ToolResultContent::Image`）、`render_artifact` / `present` / `inspect_artifact`。
- OpenAI API キーで登録した profile は、GUI が `/v1/models` を絞り込まずに取得する。画像モデルが会話用のモデルとして登録され、通常のロールのチェーンに入りうる。

## Decision

### 画像生成に使うモデルはロールのチェーンで指定する

- `agents.roles.image_generator` を追加する。`compactor` と同じく、agent としては起動しないロールのバインディングで、`models` に route のチェーンを持つ。ロールプロファイルにも同じ項目を置く。
- `models` が空なら、画像生成は無効にする。ロール名の route への暗黙のフォールバックはしない。
- チェーンの解決・フォールバック・クールダウンは ADR 0030 の仕組みをそのまま使う。たとえば Codex サブスクリプションで生成し、利用上限に達したら OpenAI API キーへ移る構成を組める。

### モデルに種別のタグを持たせる

- provider profile のモデル登録（`ModelEntryConfig`）に `kind` を加える。値は `chat`（会話用）と `image_generation`（画像生成用）。
- 省略時の種別は、モデル名から推定する。推定は固定の一覧で行う（`gpt-image-` / `chatgpt-image-` / `dall-e-` で始まる名前を `image_generation`、それ以外を `chat`）。明示したタグは推定より優先する。
- GUI でモデルを追加したとき（取得した一覧からの選択と手入力）は、推定した種別を明示のタグとして書き込む。Codex の profile では、カタログに画像モデルが載らないため、固定の一覧の画像モデル（`gpt-image-2`）を追加の候補として示す。
- route の候補が `model` を省略した場合は profile の `default_model` を使う。`default_model` は `chat` のモデルでなければならない。

### 種別の検査は両方向に行う

- 通常のロールとカテゴリのチェーンは、すべての候補が `chat` でなければならない。
- `image_generator` のチェーンは、すべての候補が `image_generation` で、かつ画像生成に対応した provider（`openai-codex` と `openai`）の profile でなければならない。
- 1 つの route に種別の違う候補を混ぜない。自動グルーピング（ADR 0030）のキーにも種別を加える。
- 厳格な読み込みではエラー、通常の読み込みでは警告にして、そのロールを使う時点で失敗させる（未定義 route の扱いと同じ）。実行時にも、解決したモデルの種別を確かめる。
- GUI のロール設定では、ロールに合う種別のモデルだけを候補に出す。

### provider の capability として実装する

- `ProviderClient` に任意の capability `image_generation()` を加える。生成と編集の 2 操作で、要求と応答は Codex と公開 API で共通の形にする。
- Codex（`openai-codex`）は ChatGPT ログインのセッションと既存の送信ヘッダーで `{codex}/images/generations` / `/images/edits` を呼ぶ。
- OpenAI（`openai`）は API キーで `{base_url}/images/generations` / `/images/edits` を呼ぶ。
- `openai-compatible` などその他の provider は対応しない。
- usage は既存の使用量イベントに記録する。

### `generate_image` ツール

- 付与の条件は `render_artifact` と同じにする（`visual` category の Worker、conversation category の root Worker）。会話の root が Orchestrator のときは、visual Worker に委譲して使う。加えて、run の開始時に `image_generator` のチェーンが設定されていなければ付与しない。
- 引数は、プロンプト、保存先、編集用の参照画像（最大 5 枚）、透過背景、縮小版の指定、任意の size / quality とする。size と quality は Codex では無視されることがあると説明に書く。
- 保存先と参照画像は、プロジェクトのワークスペースかその thread の一時領域（`$TMPDIR`）の中に限る。書き込みは `write` と同じ境界と ownership の検査を通す。
- 縮小版（長辺の上限、JPEG または PNG）を同時に保存できる。HTML モックへ埋め込むサイズに収めるためである。
- tool 結果には、保存したパスと、縮小したプレビュー画像を返す。agent は出来を見て、採用するか、参考にとどめるか、作り直すかを判断する。
- 参照画像は、会話中のモデルとは別の送り先（OpenAI）へ送られる。そのことを tool の説明と結果に明記する。設定時の同意の画面は設けない。

### `render_artifact` の拡張（ADR 0029 の変更）

- ワークスペースに加えて、その thread の一時領域のファイルも受け付ける。ADR 0029 の「workspaceまたはscratch配下」を実装に合わせる。
- HTML を取り込むとき、ワークスペースか一時領域の中を指す相対パスの画像参照（`<img src>` と CSS の `url()`）を data URI に置き換える。境界の外、存在しない、形式が合わない参照があれば、どの参照かを示して取り込みを拒否する。`http(s):` と `data:` の参照は変えない。
- 元の HTML の上限は 2 MiB のままとし、埋め込み後の上限を別に設ける。

## Consequences

- デザイン相談で、素材を生成して HTML モックに組み込めるようになる。素材がシリーズとして揃うよう、編集で画風を引き継げる。
- 画像生成は利用枠を速く消費する。どれだけ使ったかは使用量の表示で分かるが、tool 単位の上限は今回設けない。
- Codex の endpoint は公開仕様ではない。仕様が変わると Codex 経路は動かなくなり、その間は OpenAI API キー経路にフォールバックする構成で補う。
- モデル登録に `kind` が増える。省略時は名前から推定するので、既存の設定の移行は要らない。

## Open questions

- Codex 以外のクライアントから ChatGPT ログインで画像 endpoint を使うことの利用規約上の扱いは未確認である。
- Codex 経路で `model` / `size` / `quality` が実際に反映されるかは未確認である。
- `openai-compatible` の provider（画像 endpoint を持つもの）への対応は保留する。
- 生成結果を会話中のモデル以外が評価する仕組み（`MultimodalLooker` への委譲など）は必要になってから検討する。
