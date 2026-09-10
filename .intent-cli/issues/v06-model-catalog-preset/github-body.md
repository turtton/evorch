## Goal

models.dev 公開カタログ (JSON API) を取り込み、モデルメタデータ (context window / pricing / capabilities) を `ModelPreset` (手動共通設定) と自動割当のハイブリッドで管理できるようにする。provider 単位で重複していたモデル設定を解消し、同一モデルには同じ設定が自動で適用される状態にする。

## Why This Slice Exists Now

実機で `deepseek-v4-flash-0731` が 413 Payload Too Large を返した。原因は compaction engine の `context_window_tokens` が DEFAULT 200,000 で、モデルごとの実際の context window を保持・参照する仕組みがなかったこと。GUI のモデル設定画面にも context window 等の詳細設定がなく、provider 別に同一モデルの設定を個別管理するしかない構造的な不便も合わせて解消する。models.dev は認証不要の公開 JSON で、低頻度取得 + ローカルキャッシュ (24h TTL) で運用できる。

## Current Observed State

- `crates/config/src/types/provider.rs`: `ModelEntryConfig { id, enabled }` のみ (v0.7 で legacy 互換追加済み)。context window 等のメタデータは保持できない
- `crates/config/src/types/compaction.rs`: `CompactionConfig { context_window_tokens: 200_000, model_overrides: BTreeMap }` でモデル別 override は可能だが、ユーザーが手動で全モデルを列挙する必要がある
- `crates/runtime/src/compaction/policy.rs`: `resolve_window` は config の `model_overrides` のみ参照。models.dev のような外部カタログ未連携
- GUI provider 設定: モデル行は id + enabled のみ。メタデータ表示・override UI なし

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + Tokio async runtime
- v0.7 の `ModelEntryConfig` custom Deserialize (legacy 文字列互換) が確立済み — optional フィールド追加は後方互換を保てる
- v0.3/v0.4 の GUI fetch→checkbox→Apply パターンが provider 設定 UI に確立済み
- models.dev API: `GET https://models.dev/api.json` (provider→model 構造) / `models.json` / `catalog.json`。認証不要、rate limit 非公開のため低頻度 + ローカルキャッシュ前提
- OpenCode のキャッシュ実装パターン: atomic write (temp+rename)、file lock、明示/バックグラウンド refresh

## Target Repo / Path / Part

- Repo: `turtton/evorch`
- Paths: `crates/config/` (schema 拡張) / `crates/runtime/` (catalog fetch + 解決ロジック、または新規 `crates/catalog/`) / `crates/gui/` (preset 選択・override・Refresh UI)
- Part: models.dev カタログ取り込み + `ModelPreset` + モデル解決優先順位 (preset → models.dev → provider default) + GUI 連携

## In Scope

- config schema 拡張: `ModelEntryConfig` に `metadata_source` / `metadata_ref` / `preset` optional フィールド、`[model_presets.<name>]` セクション
- models.dev fetch + 24h TTL ローカルキャッシュ (OS cache dir、atomic write、file lock、stale 時バックグラウンド refresh)
- モデル解決レイヤ: 手動 preset/override > models.dev 自動割当 > provider default
- compaction `resolve_window` への正確な `limit.context` 反映
- GUI: preset 選択、解決元・解決値表示、per-model override、Refresh ボタン

## Out Of Scope

- models.dev の provider ID と evorch provider 名の自動マッピング推論 (open_questions として残す)
- provider `/v1/models` の動的 discovery 改修
- 413 の即時緩和策としての runtime request body byte ガード (必要なら別途検討)

## Standalone Child Issue Contract

- この issue は host repo の queue-state から `intent-target` として publish される。実装は child repo (`turtton/evorch` 本体) の main へ直接
- RED→GREEN 同一コミットでテストを添える

## Acceptance Criteria

- `GET https://models.dev/api.json` を fetch し、OS cache dir に atomic write + file lock で保存。TTL 24h。stale は利用しつつバックグラウンド refresh。手動 Refresh で強制 fetch
- `[model_presets.<name>]` に context_window / max_output_tokens / pricing を定義可能
- `ModelEntryConfig` の新フィールドは全て optional で、既存の文字列モデルリスト・`id+enabled` のみの既存 config がそのまま動作する
- 解決優先順位: 手動 preset/override > models.dev > provider default
- deepseek-v4-flash-0731 等で models.dev 由来の実際の `limit.context` が compaction window に反映される
- GUI でモデル行の解決値 (context window / pricing / 解決元) と override 入力、Refresh が操作可能
- `cargo test -p config` / `-p runtime` / `-p gui` 全件 green、`cargo clippy --workspace --all-targets -- -D warnings` clean、`cargo fmt --all -- --check` clean

## Verification

- 各 Phase (P1 config schema → P2 catalog fetch/cache → P3 解決ロジック → P4 GUI) ごとに RED→GREEN 回帰テスト
- P1/P2 は並行可能、P3 は P1+P2 完了後、P4 は P3 完了後
- `git diff --check` pass

## Related Links

- intents/evorch/features/context-engine/overview.md
- intents/evorch/features/provider-routing/overview.md
- intents/evorch/decisions/0003-cache-first-context-engine.md
- intents/evorch/decisions/0004-provider-routing-separation.md
- models.dev: https://github.com/anomalyco/models.dev
- OpenCode models-dev 実装: packages/core/src/models-dev.ts

## Knowledge Maintenance

- completion 時に `intents/evorch/features/context-engine/overview.md` へ v0.6 writeback を追記

## Base Branch Policy

- host repo policy に従い、実装は child repo main 直接 (PR 不要)。intent-cli 状態遷移は automation コマンド経由のみ
