# v15-b-image-model-kind Implementation Packet

## Goal

モデルに種別（`chat` / `image_generation`）を持たせ、画像生成に使うチェーン `agents.roles.image_generator` を追加する。画像モデルが通常のロールに入らないこと、画像生成のチェーンに会話用モデルが入らないことを、設定の検査・実行時の確認・GUI の候補の 3 か所で保証する。ADR 0038 の「モデルに種別のタグを持たせる」「種別の検査は両方向に行う」「画像生成に使うモデルはロールのチェーンで指定する」の節を実装する。

## Why

画像生成（v15-c）は、Codex サブスクリプションと OpenAI API キーの両方で使う。OpenAI API キーの profile は `/v1/models` の一覧をそのまま登録できるため、画像モデルの名前が取得でき、名前は更新されていく（gpt-image-2.5-* など）。名前だけで区別すると新しい名前や明示の意図を扱えないので、モデル登録に種別のタグを持たせ、省略時と GUI での追加時は固定の一覧から推定する。

## Scope

- **種別の型:** `config` に `ModelKind { Chat, ImageGeneration }`（TOML では `chat` / `image_generation`）を置く。`ModelEntryConfig.kind: Option<ModelKind>`（`#[serde(default, skip_serializing_if = "Option::is_none")]`）。strict の `MODEL_ENTRY_KEYS` に `kind` を加え、値を検査する。
- **推定:** `ModelKind::infer(model_id)` を 1 か所に置く。`gpt-image-` / `chatgpt-image-` / `dall-e-` で始まる名前（大文字小文字を区別しない）を `ImageGeneration`、それ以外を `Chat` とする。実効の種別は「明示の kind、なければ推定」。
- **候補の種別:** route の候補の実効の種別を求める関数を置く。`model` を明示した場合は、その profile の `models` に同じ id の登録があればその実効の種別、なければ名前の推定。省略した場合は `default_model` の種別。
- **profile の検査:** `default_model` が `image_generation` のモデルなら検査で不合格にする。
- **画像生成のチェーン:** `AdditionalRoleBindings.image_generator: RoleBindingConfig` を追加する。ロールプロファイルにも同じ項目が入る（`agents` を共有する型なので自動で入る）。`role_bindings` / `explicit_refs` の列挙に加え、`implicit_roles_using` からは compactor と同様に除く（暗黙のフォールバックなし）。`binding_for` は image_generator を agent のロールとして受け付けない。解決用に `AgentsConfig::image_generator_chain()` のような専用の取得口を置く。
- **両方向の検査:** `validate_chain_routes` を拡張し（関数名は実態に合わせて変えてよい）、次を検査する。
  - 通常のロール・カテゴリ・compactor のチェーンの候補はすべて `chat`。
  - image_generator のチェーンの候補はすべて `image_generation` で、profile の provider 種別が `openai-codex` か `openai`。
  - 1 つの route の候補の種別がそろっている。
  - 厳格な読み込みではエラー、通常の読み込みでは警告にする（未定義 route と同じ扱い）。エラーには該当するチェーンのアドレスと route 名、モデル名を載せる。
- **実行時の確認:** `RoutedModel` が会話の送信と要約のチェーンを解決したとき、選んだ候補の種別が `chat` か確かめ、違えば送信せずに型付きのエラーで失敗する（通常の読み込みで警告にとどめた設定を送信前に止めるため）。image_generator の解決と確認は v15-c で行う。
- **自動グルーピング:** `GroupKey` に種別を加え、種別の違うモデルを同じ route に追加しない。
- **GUI:**
  - モデル追加（`add_model`、`apply_fetched_selection` の各所）で `ModelEntryConfig` を作るときに、推定した種別を `kind` に明示して書き込む。モデル一覧の各行で kind を表示・変更できる。
  - Codex profile の編集で、固定の一覧の画像モデル（`gpt-image-2`。一覧は providers か config の定数として v15-c と共有する）を追加の候補として示す。
  - `default_model` の選択肢と `reconcile_default` から `image_generation` のモデルを除く。
  - ロール設定に「Image generator」の折りたたみを compactor の隣に加える。空のときの表示は「Disabled（generate_image は使えない）」。
  - チェーンの route 候補は、通常のロール・カテゴリ・compactor では `chat` の route、Image generator では `image_generation` の route だけを示す。route の種別は候補から決め、空の route はどちらにも示す。
  - route の編集では、すでにある候補と種別の違うモデルを候補に出さない。
  - 保存前に config の検査を通し、エラーを表示する（既存の保存の流れに従う）。
- **Schema / Docs:** `cargo run -p config --example dump_schema -- docs/config/evorch-config-v3.schema.json` でスキーマを再生成する。`docs/providers.md` に kind と推定、image_generator の設定例（Codex で生成し、上限で OpenAI API キーへ移る構成）を書く。

### 採用・却下案と理由

- 名前の判定だけにする案は採らない。新しい画像モデルや OpenAI 互換の命名を扱えず、利用者の明示の意図を表せない。
- タグを route やロールに置く案は採らない。route は同じモデルへの経路の集まりであり、種別はモデル登録から決まる。二重に持つと食い違う。
- タグを必須にする案は採らない。既存の設定の移行が必要になる。省略時は推定し、GUI での追加時に明示で書き込む。
- 種別の検査を厳格な読み込みだけにする案は採らない。通常の読み込みでも警告に加えて送信前の確認で止める。

## Out of scope

`generate_image` ツールと provider の画像 capability（v15-c）。`openai-compatible` などその他の provider の画像生成。OpenAI の `/v1/models` から embedding や音声のモデルを除く絞り込み（既存の問題として別途扱う）。モデルの kind の自動更新（推定の一覧を外部から取ること）。

## Verification

### Unit

- config: kind の serde（省略・明示・文字列だけの要素）、strict の値の検査、`ModelKind::infer`（各接頭辞、大文字小文字、境界の例 `gpt-image`・`gpt-4o-image` など）、候補の種別の決まり方（明示・登録なし・default_model）、default_model の検査、両方向の検査と route の混在、ロールプロファイル内の検査、厳格と通常の読み込みの違い、image_generator が暗黙のフォールバックを持たないこと。
- runtime: 送信と要約のチェーンで image_generation の候補を選んだ場合に送信せず失敗すること、自動グルーピングが種別の違うモデルを加えないこと。
- gui: モデル追加時の kind の書き込み、Codex の画像モデル候補、default_model の選択肢、ロール設定と route 編集の候補の絞り込み。

### Integration → build

- 既存の設定ファイル（kind なし）がそのまま読み込めること。OpenAI profile に `gpt-image-1` を含む設定で、worker のチェーンに入れると厳格な読み込みでエラーになること。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。
