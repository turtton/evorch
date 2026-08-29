# v01-routing-profiles Implementation Packet

## Goal

v0.1 のプロバイダ選択・モデル情報・設定読み込みの3層を実装する。①TOML config 層（ADR 0014: XDG 準拠 `~/.config/evorch/config.toml` + project `./evorch.toml`、優先順位 CLI 引数/環境変数 > project > user > builtin defaults、`config.d/*.toml` の辞書順 deep merge・後勝ち、version フィールド + migration、schemars による JSON Schema 生成）。②model catalog（ADR 0013: builtin デフォルト + models.dev 起動時 fetch + openai-compatible `/v1/models` 検出のマージ。検出モデルは「属性未確定フラグ」付き。subscription 系の auth 状態による動的フィルタは v0.3 対象）。③provider profile と simple fallback（ADR 0004: logical model → route → provider profile → 実モデル ID の4層解決、primary 失敗時の fallback 切り替え、ProviderCapabilities 参照）。

## Why

v0.1 の成功基準は「Orchestrator が依頼を受け role を background 起動する」ことだが、role 実行（v01-agent-roles）はどのモデル・プロバイダで動くかをこの層に委譲する。プロバイダ分離（ADR 0004）・設定アーキテクチャ（ADR 0014）・モデルカタログ（ADR 0013）は v0.1 時点で実装されなければルーティングが依存先（v01-agent-roles / v01-gui-panes）へ渡せない。また config は後続 packet 全体が利用する基盤であり、早期に立てる必要がある。

## Scope

- `crates/config/`: TOML マルチソース読み込み（XDG `~/.config/evorch/config.toml` + `config.d/*.toml` + project `./evorch.toml` + builtin defaults）、優先順位 merge、version フィールド + migration 関数、schemars による JSON Schema 生成、v0.1 設定領域（provider profiles / model routing / panel layout・keybind / diagnostics / permission preset / 計測）の typed struct
- `crates/model/`: ModelCatalog（ADR 0013 の4供給源ハイブリッドのうち v0.1 分: builtin デフォルト + models.dev 起動時 fetch（キャッシュ + オフラインフォールバック）+ `/v1/models` 検出マージ（属性未確定フラグ付き））、更新履歴の SQLite 記録、resolve 時の availability / cost / capability 参照
- `crates/routing/`: ProviderProfile 定義（Profile は credential instance を参照する、credential 自体は書かない）、logical model → route → profile → 実モデル ID の解決、simple fallback（current profile → 同じ logical model の別 profile → 別 logical model）、session affinity の基礎（同一 session の profile 留保、失敗時のみ切り替え）

## Out of scope

- subscription 系 provider（anthropic-subscription / openai-codex / github-copilot）実装と auth 状態による動的フィルタ（v0.3）
- provider 本体の呼び出し実装（v01-provider-client に委譲）
- provider health / cooldown の高度化・affinity（v0.4）
- 価格をコスト計算へ接続する部分（ADR 0012 は v0.2 以降）
- config の home-manager module（v0.2 以降。本 packet は loader の merge 機構まで）
- credential の keychain / 0600 fallback 管理（ADR 0008、v01-sandbox-approval 等の別 packet 領域）
- nix support の製品化（v0.2 以降）

## Verification

- config テスト: マルチソース読み込み（CLI > project > user > defaults の優先順位）、`config.d/*.toml` の辞書順 deep merge（後勝ち）を fixture TOML で検証
- migration テスト: version 1 → 現在 version の migration が動作し、old config が新 schema へ移行されること
- schema テスト: schemars 生成の JSON Schema が JSON としてパース可能で、`$schema` フィールドを持つ config との整合をスモークテスト
- model catalog テスト: builtin デフォルトをオフラインで返すこと。mock の `/v1/models` 応答が「属性未確定」としてマージされること
- routing テスト: mock provider で primary 失敗（429 / timeout / quota / auth）時の fallback が「同じ logical model の別 profile → 別 logical model」の順で試行されること
- `cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`git diff --check` を green にすること

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/provider-routing`（primary）。config 層は `technology/architecture` の crate 構成に対応。新規 intent node は不要（intent-tree は 00-map.md 単一構成のため feature overview を intent node として参照）
- ADR candidate: なし（decline）。model catalog は ADR 0013、config は ADR 0014 で確定済み。本 packet は実装のみ
- Diagram candidate: なし（decline）。供給源ハイブリッドと fallback 順は ADR に記述済みで、追加ダイアグラムを要求しない
- Docs update: **必要**。ADR 0014 の consequences（operations/ に config リファレンス配置予定：v0.1 実装時）に従い、`intents/evorch/operations/config-reference.md` を closeout で作成する（配置・優先順位・version migration・config.d/ の利用法を記載）。※現状 operations/ は未作成のため、この closeout 書き戻しで新設する
- Closeout learning: config マルチソース deep merge・version migration の確定仕様と、`/v1/models` 検出マージの実測知見。`write_back_required: false`（docs 書き戻しは GitHub 上で実施する）

- Guide reachability (G645): config.toml と生成 JSON Schema はオペレータ向けの設定面であり、guide の role が対向する新規の role-facing surface を追加しない。`packet.yaml` に `no_role_facing_surface: true` を明示した。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.

## Assumptions

- ADR 0013 の供給源 ④（subscription 系の auth 状態動的フィルタ）は v0.3 対象と読み取り、本 packet は ①–③ を実装する。供給源 ②（models.dev 起動時 fetch）はテストを mock で行い、実ネットワークへの接続は v01-scaffold の範疇では行わない（verification コマンドがネットワーク非依存を保証）
- 「session affinity の基礎」は本 packet では同一 session 内で profile を留保しエラー時に切り替える最小実装とし、cooldown 管理（Retry-After 優先等）は v0.4 の provider health / cooldown で拡張する前提とする
- config の credential 参照（profile 名のみ書く）は ADR 0008 の keychain 側を前提とし、本 packet では credential の内容を一切持たない