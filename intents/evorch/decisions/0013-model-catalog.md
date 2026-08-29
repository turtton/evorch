# ADR 0013: モデルカタログ — ハイブリッド4供給源

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

構想書 §8 の Logical Model / Provider Profile / capability は語られるが、モデル情報そのものの供給元は空白だった。関連する未決依存: provider-routing の解決先存在判定、ProviderCapabilities（§8.4）、コスト計算の価格カタログ（ADR 0012）、拡張の ModelCatalog domain（ADR 0010）。

既存実装の知見: pi は組み込み静的 JSON（provider ごと）、OpenCode V2 は models.dev fetch + `catalog.transform`（ModelsDevPlugin）、Codex OAuth は auth 状態でクライアント側フィルタ（ALLOWED_MODELS）。

## Decision

モデルカタログは **4供給源のハイブリッド** とする。

1. **組み込みデフォルト**: 主要モデルの属性（context window / max output / tool calling / reasoning / cache 対応）と価格。オフライン動作の基盤
2. **起動時 fetch**: models.dev 等から差分更新。キャッシュ + オフラインフォールバック（組み込みデフォルトへ退行）。鮮度とオフラインの両立
3. **プロバイダ検出**: openai / openai-compatible / openrouter 等が `/v1/models` を返す場合、カタログに無いモデルを「検出モデル」としてマージ。**属性未確定フラグ**付きで、価格・capability が判明するまでは Logical Model 解決の優先度が下がる
4. **サブスクリプション系の動的フィルタ**: anthropic-subscription / openai-codex / github-copilot は auth 状態で利用可能モデルが変わる。profile 単位でカタログを動的フィルタ（Codex ALLOWED_MODELS 方式）

### 統合設計

- ModelCatalog は ADR 0010 どおり domain transform 対象。builtin 定義も外部定義も同一 registry API
- Logical Model → route → profile → 実モデルID 解決時、カタログの availability / cost / capability を参照
- 価格カタログはコスト計算（ADR 0012）と同一ソース
- カタログの更新履歴は SQLite に保持（どの供給源がいつ更新したかの追跡）

## Consequences

- provider-routing feature に「モデルカタログ」節を追加
- v0.1 config の model routing 設定がカタログ参照になる
