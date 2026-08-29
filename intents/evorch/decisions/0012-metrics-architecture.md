# ADR 0012: 計測アーキテクチャ — OTel API + ring buffer + downsampled SQLite + 任意 OTLP export

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

構想書には cache metrics（§10.2）と usage / provider_health entity（§29）があるが、計測の収集・保存・コスト計算の設計は未確定だった。検討の中で OpenAI Codex の SQLite 問題が表面化し（下記）、「本体保存 vs 外部委譲」の議論になった。

**Codex 問題の検証結果（2026-08 調査）**: TRACE ログ sink が SSE/WebSocket イベントを1件ごと SQLite へ永続化し 15 秒で 36,211 行挿入（外挿 640 TB/年、Issue #28224）。WAL checkpoint 未実施で最大 685 GB（#22444）、freelist 未回収で DB ファイルが単調増加（#35823）。OpenCode / Cursor / Claude Code も同種の「高頻度・全量・無期限の再保存」問題を確認。**根本原因は SQLite ではなく、raw 高頻度イベントの永続化と運用ポリシー未実装の複合**。

## Decision

### 収集層

- **OTel Metrics API**（`opentelemetry` crate）を instrumentation の統一語彙とする。独自 metrics フレームワークを作らない
- **tok/s 計測**を追加: TTFT（first token latency）とストリーミング throughput は event bus の観測で自動集計（Usage イベント + タイムスタンプ）
- **コスト計算**を追加: モデル単価カタログ（input / output / cache-read / cache-write per 1M tokens）× 実使用量。session / task / agent_run / profile 単位で集計し Cost Inspector（構想 §25 Level 2）へ。サブスクリプション系は「$0 + プラン枠消費推定」表示で provider ごとに分岐

### 保存層（本体保存は downsampled のみ）

- メモリ上の **bounded ring buffer** が raw 高頻度データを保持（raw は永続化しない）
- **単一 writer** が **downsampled 集計値のみ**（例: 1分バケット per provider/model）を SQLite へバッチ書き込み。件数/時間/byte の複合閾値で flush
- metrics と生ログは分離。生 SSE / tool output / prompt 全文は metrics DB に書かない
- **ハード上限**（全経路）: 1 event 最大サイズ / 1 session 最大 / 1 日最大 bytes / WAL 最大 / DB 最大
- **WAL 運用ポリシー**: `synchronous=NORMAL` / `wal_autocheckpoint` / 定期 PASSIVE checkpoint / retention 後の予算付き `incremental_vacuum`
- **自己参照防止**: ログ DB / WAL / SHM をファイルウォッチャー・telemetry sink から明示除外
- 起動時に DB/WAL/temp サイズを検査し閾値超過で警告

### 外部委譲の位置づけ

- **OTLP exporter を optional feature** として提供。collector を持つ上級者が既存スタック（Prometheus / Grafana 等）へ流す用
- 外部委譲は「保存の置き換え」ではなく「任意の追加 sink」。GUI・自己改善・データ所有のためのローカル downsampled 保存は維持

## Consequences

- storage-memory feature の entity に downsampled metrics テーブル（1分バケット）を追加
- tools-sandbox / gui-workbench の config 公開領域に cache 閾値・metrics 保持期間・panel 表示設定を追加（ADR 0014 と連携）
- 価格カタログはモデルカタログ（ADR 0013）と同一ソース
