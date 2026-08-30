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

## ストレージ ingress の secret guard（ADR 0008 補強）

message 本文・推論・event の human-readable text が credential らしき値を含む場合、SQLite への INSERT/UPDATE/serialize より前に `storage` ingress で拒否する（issue #35）。

- **位置づけ**: ADR 0008「credential 隔離」に対する **heuristic な defense-in-depth**。完全な secret 非漏洩保証ではない（＝全形式・全来歴の secret を必ず検出するものではない）。構造上 credential を持たない型経路（typed record API、JSON key allowlist、schema 非 credential 列）に加える第 2 の堤防。
- **対象 field**: `MessageRecord.content` / `MessageRecord.reasoning`（repo::message の create/update）、永続化 Event の `MessageDelta.delta` / `ReasoningDelta.delta` / `Failed.reason` / `AgentRunStateChanged.reason` / `ExecutionDenied.reason` / `ProviderFallback.reason` / `RequestCompleted.finish_reason`（repo::event::append_event。`StorageHandle::append_event` は同規則で fail-fast 検査）。
- **検出**: 新規 dependency なしの手書き deterministic マッチャ。①限定された credential env 名（`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GITHUB_TOKEN` / `GH_TOKEN` 等、`crates/storage/src/entity.rs` の `CREDENTIAL_ENV_NAMES` に列挙。8 文字未満は過剰拒否防止で除外）からの既知値完全一致、②高シグナルな key 形状（`sk-…` / `ghp_…` 系 / `github_pat_…` / `xox[baprs]-…` / `AKIA…` / `AIza…` / private key block ヘッダ / JWT 三区分 base64url。接頭辞＋十分な長さ・字種を要求し、語中の偶然一致は直前英数字チェックで棄却）。時刻・乱数に依存しない。
- **拒否・診断方針**: `StorageError::SecretDetected { entity, field, rule }` で拒否。error / tracing / Debug 出力へ**値本体と前後コンテキストを一切含めない**（`SecretGuard` の Debug は既知値の個数のみ表示）。決定的ハッシュによる fingerprint 表現も含めない（「拒否された値が何か」を外部から照合確認できる oracle になるため）。heuristic である旨は error message にも明記する。
- **状態不変性**: 拒否された message/event は DB 行・event accounting・session total_event_bytes を一切変更しない（拒否は serialize・INSERT・accounting 更新より前）。
- **限界・非目標**: 上記 2 規則に合致しない credential 形式は検出しない、既存 DB の遡及スキャンは行わない、provider credential の取得/保管（keychain 等）や一般 logging redaction の再設計は対象外。

## Open questions

- ~~event log のスキーマ詳細（messages と tool_calls の正規化方法）~~ → 2026-08-29 解決（ADR 0018-sqlite-storage-schema、PR #12）
- memory の検索（Relevant Memory Retrieval）の実現方式
