# ADR 0038 ②: モデルの種別タグ（chat / image_generation）と画像生成ロールのチェーン

## Goal

モデル登録に種別（`chat` / `image_generation`）を持たせ、画像生成に使うチェーン `agents.roles.image_generator` を追加する。画像モデルが通常のロールに入らないこと、画像生成のチェーンに会話用モデルが入らないことを保証する。

## Why This Slice Exists Now

ADR 0038 で、画像生成を Codex サブスクリプションと OpenAI API キーの両方で使う方針に合意した。OpenAI API キーの profile は `/v1/models` の一覧をそのまま登録でき、画像モデルの名前は更新されていく。名前だけでは区別しきれないため、種別のタグを持たせ、省略時と GUI での追加時は固定の一覧から推定する。画像生成ツール（v15-c）はこのチェーンと種別に依存する。

## Current Observed State

- `ModelEntryConfig` に種別はなく、どのモデルもどのロールのチェーンに入れられる。
- OpenAI API キーの profile は、GUI が `/v1/models` を絞り込まずに取得する。
- `agents.roles.compactor` が agent として起動しないロールのチェーンの前例になっている。
- `validate_chain_routes` が未定義 route の参照を、厳格な読み込みではエラー、通常の読み込みでは警告にしている。

## Accepted Baseline You May Assume

- `kind` は任意。省略時は `gpt-image-` / `chatgpt-image-` / `dall-e-` で始まる名前を `image_generation`、それ以外を `chat` と推定する。明示が優先する。
- GUI でモデルを追加したときは、推定した種別を明示で書き込む。
- `image_generator` の `models` が空なら画像生成は無効。暗黙のフォールバックはない。
- 画像生成に対応する provider は `openai-codex` と `openai` だけ。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/config/` `crates/runtime/` `crates/gui/` `docs/` `intents/`
- Target part: `ModelKind` と推定、`agents.roles.image_generator`、両方向の検査、実行時の確認、自動グルーピングのキー、GUI のモデル追加・ロール設定・route 編集、スキーマと docs。

## In Scope

- `ModelEntryConfig.kind` と strict の検査、`ModelKind::infer`、候補の実効の種別。
- `default_model` が画像モデルを指す profile の検査。
- `agents.roles.image_generator`（ロールプロファイルを含む）と専用の取得口。
- 両方向の検査と route の種別の混在の検査（厳格はエラー、通常は警告）。
- 会話の送信と要約で、解決した候補が `chat` であることの実行時の確認。
- 自動グルーピングの `GroupKey` に種別を加える。
- GUI: 追加時の kind の書き込みと変更、Codex の画像モデル候補、default_model の選択肢、ロール設定の Image generator、種別による候補の絞り込み。
- スキーマの再生成、`docs/providers.md`。

## Out Of Scope

`generate_image` と provider の画像 capability（v15-c）、`openai-compatible` の画像生成、`/v1/models` から embedding や音声のモデルを除く絞り込み、推定の一覧の外部からの更新。

## Acceptance Criteria

1. `kind` を省略した設定と文字列だけの要素は従来どおり読み込める。移行は要らない。
2. 実効の種別は明示の `kind`、なければ固定の一覧による推定。推定は 1 か所の関数にまとめる。
3. route の候補の種別は、`model` を明示すればその登録（無ければ名前の推定）、省略すれば `default_model` から決まる。
4. `default_model` が画像モデルの profile は検査で不合格になる。
5. `agents.roles.image_generator` を追加する。空なら画像生成は無効で、暗黙のフォールバックはない。
6. 通常のロール・カテゴリ・compactor に画像モデル、image_generator に会話用モデルか非対応 provider、1 つの route に種別の混在があれば、厳格な読み込みでエラー、通常の読み込みで警告になり、使う時点で失敗する。
7. 会話の送信と要約で解決した候補が `chat` でなければ、送信せずに失敗する。
8. 自動グルーピングは種別の違うモデルを同じ route に加えない。
9. GUI でモデルを追加したとき、推定した種別を `kind` に書き込む。利用者は変更できる。
10. GUI の Codex profile の編集で `gpt-image-2` を追加の候補として示す。
11. GUI の `default_model` の選択と `reconcile_default` は画像モデルを選ばない。
12. GUI のロール設定に Image generator を加え、各チェーンには種別の合う route だけを示す。route の編集で種別を混ぜられない。
13. 設定のスキーマを再生成する。
14. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: config の serde・推定・候補の種別・両方向の検査・厳格と通常の違い、runtime の送信前の確認と自動グルーピング、gui の追加・候補の絞り込み。
2. Integration: kind なしの既存設定が読み込めること、`gpt-image-1` を worker のチェーンに入れた設定が厳格な読み込みでエラーになること。
3. Build / lint: `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`scripts/check-cache-contracts.sh`、`git diff --check`。

## Related Links

- `intents/evorch/decisions/0038-image-generation.md`
- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/features/provider-routing/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
