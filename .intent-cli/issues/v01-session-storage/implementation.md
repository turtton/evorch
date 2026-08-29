# v01-session-storage Implementation Packet

## Goal

`crates/storage/` に SQLite ベースの session 永続化を実装する。event-sourced な持久化（sessions / tasks / messages / agent_runs / events）と migration を提供し、ADR 0012 に従って raw 高頻度計測は直接書かず single-writer + バッチ flush で downsampled metrics（1分バケット per provider/model）のみ永続化する。WAL 運用ポリシー（synchronous=NORMAL / wal_autocheckpoint / 定期 PASSIVE checkpoint）とハード上限を PRAGMA 初期化および設定として適用する。credential は永続化しない（ADR 0008）。SQLite アクセス層は本 packet で rusqlite に確定し、schema と運用ポリシーを ADR 0018 として記録する。

## Why

storage-memory feature は「SQLite 中心の event-sourced runtime。Event Log を source of truth とし state projection → GUI で resume / branch / rewind を可能にする」ことを要件とし、受け入れ基準に「全 event が SQLite に追記され、session 中断後に resume できること」を含む。v0.1 成功基準も「session が SQLite に永続化される」ことを要求する。architecture.md は Storage を「SQLite」とだけ定めており、アクセス層の選択とスキーマ詳細は未確定。また ADR 0012（Codex の 640 TB/年を踏まえた downsampled + WAL 運用）と ADR 0008（credential 隔離）を実装に落とす必要があり、後続の agent-roles / provider-client / gui-panes slice はこの永続化層に依存するため、v0.1 の早い段階で固定する。

## Scope

- `crates/storage/` に以下を実装:
  - SQLite アクセス層: **rusqlite**（`bundled` 相当の組み込み SQLite）を採用。ADR 0012 の single-writer 方針に合わせ、専用 writer タスク（tokio 側からは channel / `spawn_blocking` 経由）が唯一の書き込み経路を持つ。
  - migration: version 管理された SQL migration（例: `schema_migrations` または `PRAGMA user_version`）を起動時に適用。
  - tables:
    - `sessions` / `tasks` / `messages` / `agent_runs` / `events`（event-sourced の domain event log。messages と tool_calls の正規化は ADR 0018 で確定した形に従う）
    - `downsampled_metrics`（1分バケット per provider/model: bucket_ts / provider / model / input_tokens / output_tokens / cache_read / cache_write / requests / ttft_sum 等、ADR 0012 の集計列）
  - CRUD レイヤー: session / task / message / agent_run / event の insert / read / update / delete。
  - 復元: events から session の状態を再現できる投影（read path）。再起動後の session resume をテストで保証。
  - ADR 0012 の運用:
    - PRAGMA: `journal_mode=WAL` / `synchronous=NORMAL` / `wal_autocheckpoint` 設定 / 定期 PASSIVE checkpoint / retention 後の予算付き `incremental_vacuum`
    - ハード上限: 1 event 最大サイズ / 1 session 最大 / 1 日最大 bytes / WAL 最大 / DB 最大 を設定値として持ち、超過時に拒否または警告
    - 起動時安全検査: DB / WAL / temp サイズを検査し閾値超過で警告
    - 自己参照防止: DB / WAL / SHM をファイルウォッチャー / telemetry sink から除外する設計（storage 側は把握のみ、SI は config 側で適用）
  - metrics 書込経路: v01-event-stream の in-memory 集計スナップショットを消費し、single-writer が downsampled 行をバッチ flush する。raw 高頻度イベントの直接書込みはしない。
  - credential 非永続化: 公開型に API key 等を含めず、`credential が languages 的に書き込めない`設計 + テスト。
- ADR 0018（SQLite アクセス層・schema・WAL 運用）を生成。

## Out of scope

- memory パイプライン（quick agent による knowledge 抽出と persistent memory 保存。v0.4 想定）。
- provider_health テーブル（routing の cooldown 判断用。v0.2 想定の entity。必要なら後続 slice）。
- credential の保存・管理（keychain / 0600 JSON。ADR 0008 の方針どおり storage の外側 / providers 側の担当）。
- 外部 metrics 委譲（OTLP exporter は任意の追加 sink、後続 slice）。
- GUI からの投影・表示（v01-gui-panes の担当）。

## Verification

- unit test:
  - migration 適用で全テーブルが作成される（テーブル一覧の確認）。
  - session / task / message / agent_run / event の CRUD。
  - セッション生成 → イベント追記 → ストレージ再オープン → 復元、が一連で通る（resume 検証）。
  - downsampled_metrics の集計書込み経路（in-memory バケット → バッチ flush → テーブル行）が通る。
  - ハード上限超過時に書込みが拒否される。
  - credential 相当の値を型経由で書き込めないことのコンパイル / テスト検証。
- `cargo test -p storage` / `cargo clippy -p storage -- -D warnings` / `cargo fmt --check` / `git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `intents/evorch/features/storage-memory/overview.md` を具体化する slice（event-sourced 永続化）。新規 intent node は不要。
- ADR candidate: 必須採用。`intents/evorch/decisions/0018-sqlite-storage-schema.md`「SQLite アクセス層（rusqlite・single-writer）と storage schema / WAL 運用を確定する」を生成し、storage-memory overview の Open questions「event log のスキーマ詳細」を解決済みにする。
- Diagram candidate: なし（event log → state projection → GUI の流れは storage-memory overview で既述。schema 詳細は ADR 0018 に記録）。
- Docs update: `intents/evorch/features/storage-memory/overview.md` の「Open questions」および「計測の保存（ADR 0012）」節を ADR 0018 決定に合わせ更新。
- Closeout learning: rusqlite 選択の根拠（single-writer task に sync 接続を閉じ込める設計が ADR 0012 のバッチ flush / ロック回避と整合。async 対応の必要がある場合は spawn_blocking で対応）と、messages / tool_calls の正規化方針、WAL 運用の実測値を記録。`write_back_required: true`。

- Guide reachability (G645): 本 slice は内部 crate のみで、ユーザー / オペレータ向け等の role-facing surface を追加しない。`no_role_facing_surface: true` を明示する。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.