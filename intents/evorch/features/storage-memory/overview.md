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

## 保守 maintenance（incremental vacuum / temp 容量診断、ADR 0012 完成）

issue #37 で確定した実装値と挙動:

- **auto_vacuum=INCREMENTAL 初期化/移行**: DB open 時、他 pragma（特に `journal_mode=WAL`）が空ファイルのヘッダを初期化する前に `page_count` を検査する。新規 DB（`page_count == 0`）ではテーブル作成前に `auto_vacuum=2` を有効化する。**既存 DB の `auto_vacuum=FULL`(1) は `INCREMENTAL` へ移行する**: FULL は pointer-map 構造を既に持つため full VACUUM なしで安全に変更できる（`auto_vacuum migrated: FULL -> INCREMENTAL` を info ログ出力）。**既存 DB で auto_vacuum 未設定(0)・既にページを持つものは変更しない**: 反映に full VACUUM（DB 全体の再書き込み）が必要で、起動時に既存接続を長時間 block する破壊的移行になるため。代わりに起動時 info ログで「incremental vacuum は手動 VACUUM まで非アクティブ」と通知する。`PRAGMA incremental_vacuum` は auto_vacuum=0 の DB では no-op なので、既存 DB でも maintenance パスは安全。
- **vacuum trigger**: maintenance tick（checkpoint 間隔に同調、既定 60 秒）ごとに `PRAGMA freelist_count` を読み、`StorageConfig.vacuum_freelist_threshold_pages`（既定 **1_024 page**、既定 page サイズ 4KiB 時 ≈ 4 MiB）以上のときだけ回収を実行する。未満では一切実行しない。
- **page budget**: 1 tick 当たり `StorageConfig.vacuum_page_budget_per_tick`（既定 **256 page** ≈ 1 MiB）を `PRAGMA incremental_vacuum(N)` に渡す。budget が回収量の上限であり、通常 read/write を無制限に block しない（最悪でも tick あたり 1 MiB 分の回収に留まる）。`0` で回収を無効化できる。既定値は ADR 0012 の方針（write/read を圧迫しない保守的量）に従う: 60 秒 tick で時間あたり最大 ≈ 60 MiB の freelist 回収速度となり、Codex 型の単調増加を防ぎつつ各 tick の滞留時間を小さく抑える。
- **vacuum 診断**: 実行 tick ごとに回収前後の freelist page 数・回収 page 数・budget を `tracing::info!("incremental vacuum completed", freelist_before_pages, freelist_after_pages, pages_reclaimed, page_budget)` として出力する（secret-free）。
- **temp 容量診断**: evorch 管理対象 temp 副産物 = rollback journal `<db>-journal` の合計バイト数（現行スコープ。db/-wal/-shm は `file_sizes` の責務で二重計上しない。OS 全体の temp 監視は issue の Out of Scope により対象外）。起動時と maintenance tick ごとに測定し、`StorageConfig.temp_warn_bytes`（既定 **256 MiB**）以上で `<db>-journal` が実在する場合に `tracing::warn!("temp storage threshold exceeded", temp_bytes, threshold_bytes)`、復帰時に `tracing::info!("temp storage within threshold", ...)` を出力する。警告は**閾値の超過/復帰の遷移時にのみ 1 回**（state transition ベースの重複抑制）で、threshold 未満が継続する限り何も emit しない。なお既存 DB 上の stale `-journal` は open 時も残存して測定されるが、fresh DB に先行する孤立 journal は SQLite 自体が初期化時に除去する（実測確認済み）。
- **既存動作の維持**: PASSIVE/TRUNCATE checkpoint・DB/WAL hard limit・event/metrics 動作は不変。`StorageHandle::checkpoint_now()` は「PASSIVE checkpoint + サイズ再評価 + 上記 maintenance」を即時実行する maintenance tick としても使える。

## Open questions

- ~~event log のスキーマ詳細（messages と tool_calls の正規化方法）~~ → 2026-08-29 解決（ADR 0018-sqlite-storage-schema、PR #12）
- memory の検索（Relevant Memory Retrieval）の実現方式
