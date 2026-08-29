## Goal

v0.1 のプロバイダ選択・モデル情報・設定読み込みを実装する。①TOML config 層（ADR 0014: XDG `~/.config/evorch/config.toml` + project `./evorch.toml`、優先順位 CLI 引数/環境変数 > project > user > defaults、`config.d/*.toml` 辞書順 deep merge ・後勝ち、version フィールド + migration、schemars による JSON Schema 生成）。②model catalog（ADR 0013: builtin デフォルト + models.dev 起動時 fetch + `/v1/models` 検出マージ、検出分は属性未確定フラグ付き。subscription 動的フィルタは v0.3）。③provider profile と simple fallback（ADR 0004: logical model → route → profile → 実モデル ID の解決、primary 失敗時の fallback、ProviderCapabilities 参照）。

## Why This Slice Exists Now

role 実行（v01-agent-roles）はどのモデル・プロバイダで動くかをこの層に委譲するため、ルーティングが無いと runtime が成立しない。また config（ADR 0014）と model catalog（ADR 0013）は v0.1 時点で実装されなければ後続 packet 全体の基盤が空になる。mvp-roadmap v0.1 の「provider profile / simple fallback」はこの層で充足される。

## Current Observed State

greenfield。`crates/routing/`・`crates/model/`・`crates/config/` は v01-scaffold による空 crate のみ。config 読み込み・model catalog・routing の実装は存在しない。ADR 0004 / 0013 / 0014 が設計を確定済みだがコード未着手。

## Accepted Baseline You May Assume

- v01-scaffold により `crates/routing/`・`crates/model/`・`crates/config/` が空 crate として Scaffold 済み
- v01-provider-client が ProviderProfile 抽象（type / credential instance / API protocol）と ProviderCapabilities 型を提供する
- v01-session-storage が SQLite を提供し、model catalog の更新履歴の保持に利用できる

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/routing/`, `crates/model/`, `crates/config/`

Target part: `プロバイダ選択・フォールバック・モデル情報・設定読み込み`

## In Scope

- `crates/config/`: TOML マルチソース読み込みと優先順位 merge、`config.d/*.toml` の辞書順 deep merge（後勝ち）、version フィールド + migration 関数、schemars による JSON Schema 生成、v0.1 設定領域の typed struct（provider profiles / model routing / panel layout・keybind / diagnostics / permission preset / 計測）
- `crates/model/`: ModelCatalog（builtin デフォルト + models.dev 起動時 fetch（キャッシュ + オフラインフォールバック）+ `/v1/models` 検出マージ（属性未確定フラグ付き））、更新履歴の SQLite 記録、resolve 時の availability / cost / capability 参照
- `crates/routing/`: ProviderProfile 定義（credential は参照のみ）、logical model → route → profile → 実モデル ID の解決、simple fallback（current profile → 同じ logical model の別 profile → 別 logical model）、session affinity の基礎

## Out Of Scope

- subscription 系 provider（anthropic-subscription / openai-codex / github-copilot）実装と auth 状態による動的フィルタ（v0.3）
- provider 本体の呼び出し実装（v01-provider-client に委譲）
- provider health / cooldown の高度化（v0.4）
- 価格のコスト計算接続（ADR 0012、v0.2 以降）
- config の home-manager module（v0.2 以降）
- credential 管理（keychain / 0600 fallback。ADR 0008 の別領域）

## Standalone Child Issue Contract

`crates/config/`・`crates/model/`・`crates/routing/` に、v0.1 の設定・モデル・ルーティング3層を実装する。config は TOML をマルチソース（CLI 引数/環境変数 > project `./evorch.toml` > user `~/.config/evorch/config.toml` > builtin defaults）で読み込み、`config.d/*.toml` を辞書順 deep merge で後勝ちマージし、version フィールドと migration 関数を持ち、schemars で JSON Schema を生成・公開する。model catalog は builtin デフォルトをオフラインで返し、mock の `/v1/models` 検出を属性未確定フラグ付きでマージする（models.dev 起動時 fetch はキャッシュ + builtin フォールバック、テストは mock）。routing は TOML で複数の provider profile を定義でき、logical model → route → profile → 実モデル ID へ解決し、primary profile の失敗（429 / 5xx / timeout / quota / auth）時に simple fallback（同じ logical model の別 profile → 別 logical model）へ切り替える。credential は config に書かず、参照のみ扱う。subscription 系の動的フィルタと cooldown 高度化は対象外。

## Acceptance Criteria

- TOML config が CLI > project > user > defaults の優先順位でマルチソース読み込みされ、`config.d/*.toml` を辞書順 deep merge（後勝ち）できる
- config に version フィールドがあり、version migration 関数で旧 config を新 schema へ移行できる
- schemars で config 構造体から JSON Schema が生成される（公開ファイルとして出力）
- model catalog が builtin デフォルトをオフラインで返し、起動時 fetch は失敗時 builtin へフォールバックする
- model catalog が `/v1/models` 検出結果を「属性未確定フラグ付き」でマージできる（mock provider によるテスト）
- provider profile を TOML で複数定義でき、logical model → route → profile → 実モデル ID へ解決できる
- primary profile 失敗時に simple fallback（同じ logical model の別 profile → 別 logical model の順）へ切り替わる
- subscription 系の auth 状態による動的フィルタが v0.1 対象外であることが計画上明示される

## Verification

- config テスト: マルチソース読み込みの優先順位と `config.d/*.toml` の辞書順 deep merge（後勝ち）を fixture TOML で検証
- migration テスト: version 1 → 現在 version への移行が動作すること
- schema テスト: schemars 生成 JSON Schema がパース可能であること
- model catalog テスト: builtin オフライン応答と mock `/v1/models` の属性未確定マージ
- routing テスト: mock provider で primary 失敗時の fallback 順を検証
- `cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`git diff --check` を green にすること

## Related Links

- [features/provider-routing/overview.md（モデルカタログ節）](../../../intents/evorch/features/provider-routing/overview.md)
- [ADR 0004: Provider Type / Profile / Logical Model / API Protocol の分離](../../../intents/evorch/decisions/0004-provider-routing-separation.md)
- [ADR 0013: モデルカタログ — ハイブリッド4供給源](../../../intents/evorch/decisions/0013-model-catalog.md)
- [ADR 0014: 設定アーキテクチャ — TOML・マルチソース・versioned schema](../../../intents/evorch/decisions/0014-config-architecture.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/provider-routing`（primary）。新規 intent node 不要
- ADR candidate: none（ADR 0013 / 0014 で確定済み）
- Diagram candidate: none
- Docs update: **必要** — ADR 0014 に従い closeout で `intents/evorch/operations/config-reference.md` を作成（配置・優先順位・version migration・config.d/ 利用法）
- Closeout writeback expected: no（docs 書き戻しは本 packet の closeout 作業として実施）

## Guide Reachability (G645)

本スライスが追加するのはオペレータ向けの設定面（config.toml と生成 JSON Schema）であり、guide の role が対向する新規の role-facing surface（CLI / GUI / 公開契約）ではない。`no_role_facing_surface: true` とする。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.