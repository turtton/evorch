# v06-model-catalog-preset Implementation Packet

## Goal

models.dev カタログを fetch + 24h キャッシュし、`ModelPreset` (手動共通設定) と組み合わせてモデルメタデータ (context window / pricing / capabilities) を解決する。compaction が正確な per-model context window を使えるようにし、provider 別の重複設定を解消する。

## Why

実機で `deepseek-v4-flash-0731` が 413 Payload Too Large を返した。compaction の `context_window_tokens` が DEFAULT 200k 固定で、モデル実際の limit を参照する仕組みがないため。GUI にもモデル詳細設定がなく、同一モデルを provider ごとに個別設定するしかない。

## Scope

- P1 config schema: `ModelEntryConfig` へ `metadata_source` / `metadata_ref` / `preset` (全て optional) と `[model_presets.<name>]` セクション追加。legacy 文字列/id+enabled 互換維持
- P2 ModelCatalog: `GET https://models.dev/api.json` fetch、OS cache dir (`~/.cache/evorch/models-dev.json`)、TTL 24h、atomic write (temp+rename)、file lock、stale 利用 + バックグラウンド refresh、timeout 10s
- P3 解決ロジック: 手動 preset/override > models.dev > provider default。`compaction::policy::resolve_window` に catalog 由来の `limit.context` を反映
- P4 GUI: モデル行に preset 選択、解決元/解決値表示、override 入力、Refresh ボタン

## Out of scope

- models.dev provider ID ↔ evorch provider 名の自動マッピング
- provider `/v1/models` discovery 改修
- request body byte ガード (413 即時緩和)

## Verification

各 Phase で RED→GREEN 回帰テスト同一コミット。`cargo test -p config` / `-p runtime` / `-p gui` 全件 green、`cargo clippy --workspace --all-targets -- -D warnings` clean、`cargo fmt --all -- --check` clean、`git diff --check` pass。P1/P2 並行可、P3 は P1+P2 後、P4 は P3 後。

## Knowledge Maintenance (G461, optional)

- Intent placement: context-engine (cache-first 設計の補完データ取り込み)
- ADR candidate: モデルメタデータ解決優先順位 (preset > catalog > default) が ADR-worthy かは実装後に判断
- Docs update: config schema dump (`docs/config/evorch-config-v2.schema.json` regen) が必要
