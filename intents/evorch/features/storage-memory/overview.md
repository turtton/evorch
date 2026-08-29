# Feature: Storage & Memory（ストレージとメモリ）

[features 一覧](../) / [context-engine](../context-engine/overview.md) / [architecture](../../technology/architecture.md)

## 概要

SQLite を中心とした event-sourced runtime とする。Event Log を source of truth とし、state projection → GUI という流れで resume / branch / rewind / timeline / debugging / usage analysis を可能にする。

## 要件

- **主な entity**: sessions / tasks / agent_runs / messages / tool_calls / events / usage / diagnostics / artifacts / memory / provider_health
- **Event sourcing**: event log → state projection → GUI。全履歴が残り、rewind / branch が可能
- **Memory パイプライン**: task / session 終了時に quick agent が「今回の作業から将来も有用な知識は何か」を抽出し persistent memory へ保存。session 途中で stable prefix に挿入せず、次の task boundary から利用
- **Session / Task 構造**: Session より下に Task 境界。Task A（調査）→ compact → Task B（実装）→ compact → Task C（テスト改善）のように長寿命 workspace として使う

## 受け入れ基準

- 全 event が SQLite に追記され、session 中断後に resume できること
- memory が task boundary で stable prefix に snapshot として反映されること
- provider_health の記録が routing の cooldown 判断に使えること

## Related decisions

- [ADR 0003: Cache-first Context Engine](../../decisions/0003-cache-first-context-engine.md)

## 計測の保存（ADR 0012）

Codex の SQLite 問題（raw 高頻度イベントの永続化で 640 TB/年規模）の教訓を踏まえ、計測は「メモリ ring buffer で raw 保持 → downsampled 集計値のみを単一 writer がバッチ書き込み」。生 SSE / tool output / prompt 全文は永続化しない。WAL 運用ポリシー・ハード上限・自己参照防止・起動時安全検査を実装。外部委譲は optional OTLP export（追加 sink）。

## Open questions

- event log のスキーマ詳細（messages と tool_calls の正規化方法）
- memory の検索（Relevant Memory Retrieval）の実現方式
