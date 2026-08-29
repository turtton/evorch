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

## Open questions

- event log のスキーマ詳細（messages と tool_calls の正規化方法）
- memory の検索（Relevant Memory Retrieval）の実現方式
