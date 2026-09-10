# v06-model-catalog-preset Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections;
- breaks legacy config compatibility (文字列モデルリスト / id+enabled のみのエントリ);
- copies models.dev データを config に大量複写する (catalog は実行時参照のみ);
- disables the 24h cache or fetches models.dev on every request.

## Facet context

### vocabulary

- ModelCatalog: models.dev 由来の実行時参照専用カタログ。config には保存しない
- ModelPreset: `[model_presets.<name>]` のユーザー定義共通設定
- metadata_source: manual | models-dev | provider-default

### invariant

- 解決優先順位: 手動 preset/override > models.dev > provider default
- cache は atomic write (temp+rename) + file lock
- stale cache は利用しつつバックグラウンド refresh (失敗時は stale を保持して error 表示のみ)

### decider

- compaction window は catalog の limit.context を優先し、なければ config context_window_tokens (default 200k) にフォールバック
