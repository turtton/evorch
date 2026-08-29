# ADR 0018: SQLite アクセス層（rusqlite・single-writer）と storage schema / WAL 運用を確定する

## Status

Accepted（2026-08-29、PR #12 で実装確定。issue #3 / v01-session-storage）

## Context

architecture.md は Storage = SQLite とするのみでアクセス層が未確定だった。storage-memory feature の受け入れ基準（全 event が SQLite に追記され中断後 resume 可能）と、ADR 0012（single-writer + downsampled のみ永続化 + WAL 運用）、ADR 0008（credential 隔離）を実装に落とす必要があった。storage-memory/overview.md の Open question「event log のスキーマ詳細」も同時に解決する。

## Decision

### アクセス層: rusqlite（bundled）・single-writer

- `rusqlite = { version = "0.40", features = ["bundled"] }` を `[workspace.dependencies]` に集約。bundled は libsqlite3 C ソースを同梱コンパイルするため C compiler が必要（ubuntu-latest CI・nix devShell で検証済み）。初回ビルドは数分かかる
- tokio 非依存。single-writer は専用 OS スレッド + `std::sync::mpsc::sync_channel`（`rusqlite::Connection` は `Send + !Sync` のため writer スレッドのみが所有）。`event_bus::UsageSink` が同期 trait のため成立する

### schema（migration v1、全時刻は epoch ナノ秒 i64）

`sessions` / `tasks` / `messages` / `agent_runs` / `events` / `downsampled_metrics` の 6 テーブル。

- `events` は rowid（`id INTEGER PK`）= 追記順・projection 再生順の append-first log。`session_id` は **FK なし**（先行追記を許容）。`payload`（`serde_json(EventKind)`）が真実源
- `downsampled_metrics` は `event_bus::UsageBucket` のフィールドと 1:1。PK `(window_start, provider, model)`（1分バケット per provider/model、ADR 0012）。`UsageEvent.Usage` / `CacheStats` は集計器側で同一バケットにマージしてから単一トランザクションの additive upsert（`ON CONFLICT ... DO UPDATE`）で書く
- migration は `PRAGMA user_version` 管理。実装より新しい DB は `SchemaTooNew` で open を拒否

### WAL 運用（ADR 0012 の実装値）

- 全 open 時: `journal_mode=WAL` / `synchronous=NORMAL` / `wal_autocheckpoint=1000`（ページ）/ `foreign_keys=ON` / `busy_timeout=5000ms`
- 定期 PASSIVE checkpoint を既定 60 秒間隔で実行。WAL が上限超過時は 1 回だけ `wal_checkpoint(TRUNCATE)` に escalate + warn

### ハード上限（`HardLimits` 既定値）

| 種別 | 値 | 拒否時挙動 |
|---|---|---|
| 1 event | 256 KiB | 追記拒否 |
| 1 session 累積 | 64 MiB | 追記拒否（`OCTET_LENGTH` 実バイト計上） |
| 1 日あたり（UTC） | 256 MiB | 追記拒否 |
| WAL | 64 MiB | checkpoint tick で TRUNCATE escalate |
| DB 合計（db+-wal+-shm） | 1 GiB | 起動時・checkpoint 時判定 |

- 起動時サイズ検査: 合計 ≥ 1 GiB であっても **open は成功**（読み取り・復元・resume を維持）し `writes_suspended=true` となりイベント追記のみ拒否。CRUD / metrics は継続、suspended 中は上限未満に戻れば自動解除
- 合計 ≥ 80% (`soft_warn_ratio`) で `tracing::warn!`（状態遷移時のみ）
- 自己参照防止: `storage::watch_exclusions(db_path)` が `[db, db-wal, db-shm, db-journal]` の絶対パスを返す。将来のファイルウォッチャーが consume する

### credential 非永続化（ADR 0008）

- **型レベル**: 全書き込み経路が型付きレコードのみを受け付ける。`append_event` は `event_bus::Event`（closed enum、serde 隣接タグ）のみ、entity CRUD は固定フィールドのレコード型のみ、metrics は `UsageBucket` のみ。生 SQL / 任意 key-value map を受ける公開 API は存在しない
- **テスト**: 6 テーブルの column 完全 snapshot、column 名 denylist（`secret/password/credential/api_key/...`）、全 14 イベントバリアントの JSON key allowlist

### projection（event log → session 復元）

`events` を `id` 昇順で純粋 fold し `SessionSnapshot`（status / pending_message / pending_reasoning / open_tool_calls / task_ids 等）を再構成。帰属は envelope の `session_id` のみ。`reconcile(conn)` が fold 結果を sessions / tasks 行へ単一トランザクションで冪等 upsert。resume 手順: open → `restore_sessions` → `reconcile`。

## Consequences

- storage-memory/overview.md の「event log のスキーマ詳細」Open question は解決
- events.payload が真実源のため projection は常に再構築可能。ただしイベント語彙に finalize イベントがないため、`pending_message` / `pending_reasoning` は全 delta の累積（完了セッションでは会話全文を含む）— これはイベント語彙への finalize 追加を検討する契機
- memory の検索（Relevant Memory Retrieval）は v0.4 で別途確定（overview の Open question に残存）
- `provider_health` テーブルは v0.2 で追加予定
