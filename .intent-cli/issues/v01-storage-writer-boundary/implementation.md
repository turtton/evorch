# v01-storage-writer-boundary Implementation Packet

## Goal

`crates/storage/` のSQLite mutationを専用single-writerの所有権へ型で閉じ込める。repo/Connectionを外部crateへ露出せず、必要なread pathは明示的なread-only APIとして維持し、正規mutationはwriter handle/capabilityだけを通る構造にする。

## Why

issue #3 / `v01-session-storage` のv0.1 inspectで、single-writerが規約コメントに留まり型で強制されていないことが判明した。`crates/storage/src/lib.rs:10-17` は `db` と `repo` をpublicにし、`crates/storage/src/repo/mod.rs:1-12` は「single-writer管理下のみ」と説明しながらmutation関数をpublic exportする。`Database.conn` 自体はcrate-private (`db.rs:16-29`) だが、外部crateは別のrusqlite Connectionを作ってpublic repoへ渡せる。ADR 0018 (`0018-sqlite-storage-schema.md:13-16`) のwriter threadだけがConnectionを所有する決定をv0.1.1で実効化する。

## Scope

- `repo` と、mutation実装に必要な `db` / migration internalsをcrate-privateにするか、外部からmutationを構築できないsealed capabilityで保護する。
- `Storage` が唯一のwriter thread/Connection ownerとなり、clone可能な `StorageHandle` はtyped command送信だけを提供する。
- event append、downsampled metrics、catalog update、および必要なentity mutationをwriter command経由へ統合する。公開Connectionや任意SQL escape hatchは作らない。
- session復元・一覧・entity取得等、利用中のread pathはread-only facade/handleで公開する。API名は既存consumerを探索して最小変更にする。
- 外部crateからrepo mutationをimport/executeできないことをtrybuild等のcompile-fail test、doc compile test、または独立integration fixtureで証明する。
- writer経由mutationとread facadeのruntime回帰testを追加する。

## Out of scope

- SQLite schema/migration、WAL policy、hard limits、projection semanticsの変更。
- retention/downsampling/maintenance policyの変更。
- raw usage ingress guardの実装はbacklog 4 `v01-storage-metrics-ingress-guard`。同packetが `writer.rs` / `repo/event.rs` に先行変更を入れるため、本sliceはその後に適用し、visibility refactorでraw guardを落とさない。
- provider/config/GUI側の新機能。

## Verification

- compile-fail verificationで外部crateから `storage::repo::*::{create,update,delete,append_event,upsert_buckets}` 等へ到達できないことを確認する。
- `cargo test -p storage` でwriter経由event/metrics/catalog/entity mutationとread-only API、resume projectionを検証する。
- workspace consumerにAPI変更が波及する場合は該当crate testも実行する。
- `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: storage-memoryの既存single-writer実装を強化。新規node不要。
- ADR candidate: なし。ADR 0018の決定を型で実装する。
- Diagram candidate: なし。
- Docs update: なし。内部Rust API境界のみ。
- Closeout learning: writer capability/read-only facadeの形とcompile-fail証拠をcloseoutへ記録。`write_back_required: false`。
- Guide reachability (G645): role-facing surfaceなし。`no_role_facing_surface: true`。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.

## 実装確定（2026-08-30、PR #28 / issue #27）

ADR 0018 の決定を型境界として実装した（squash commit `ef9c569`）。

- **公開境界**: storage crate の module visibility を引き締め（`db` / `migrations` / `projection` / `repo` / `writer` は private、`config` / `entity` / `error` のみ `pub mod`）。公開 re-export は `Database` / `Storage` / `StorageHandle` / `system_time_to_ns` 等の時刻変換 / `CatalogUpdateRecord` / `ReconcileSummary` / `SessionSnapshot` / `StoredEvent`。
- **read-only facade（13 method）**: `crates/storage/src/read.rs` で `impl Database` に session/task/message/agent_run 系 get・list、`events_by_session` / `events_all_ordered` / `metrics_range` / `restore_session(s)` を実装。`Database.conn` は `pub(crate)` のまま外部不可視。
- **write は writer command のみ**: `StorageHandle` に `record_catalog_update(CatalogUpdateRecord)` と `reconcile() -> ReconcileSummary` の 2 command を追加（`crates/storage/src/writer.rs`、`Command::RecordCatalogUpdate` / `Command::Reconcile`）。writer 内部状態は `writer/state.rs` へ分離（挙動は line-for-line 維持）。
- **repo CRUD の意味づけ**: `repo/*` の create/update/delete は production から非到達となったため `#[cfg_attr(not(test), expect(dead_code, reason = ...))]` で明示し、in-crate 契約テスト（`repo/crud_tests.rs` / `credential_tests.rs` / `event/limits_tests.rs` へ移動）で担保を継続。未使用の pub wrapper `session_event_bytes` / `day_event_bytes` は削除。
- **Database::open は read + schema-init の正規境界として公開継続**（mutation method はゼロ、`record_catalog_update` メソッドは削除済み）。schema-init が DDL を書きうる残留リスクは PR #28 body に明記。
- **compile-fail 証拠**: `lib.rs` doctest 5 件（`storage::repo` / `storage::repo::session::create` / `storage::projection::reconcile` の E0603、`db.conn` の E0616、`StorageHandle(_)` destructure の E0532）で外部到達不能を固定。
- **consumer 移行**: `model::refresh` の履歴記録は `StorageHandle::record_catalog_update` 経由。storage 各 integration test（credential_surface / resume / raw_usage_guard / usage_flush / catalog_history）は公開経路（Storage + Database facade）へ移管され、assertion parity を保持（crud 22→22 / limits 14→14 / credential 12→12 / raw usage 4→4）。新規テスト: `tests/read_api.rs`（facade 13 method 全通り）、`tests/writer_commands.rs`（catalog / reconcile の writer 経由 roundtrip）。
- raw usage 直接永続化ガード（issue #25）は handle・repo 両レベルで維持（repo レベルは in-crate `repo/event/tests.rs::repo_append_event_rejects_raw_usage_without_increasing_row_count` へ移動）。

writeback: packet は `write_back_required: false`（intent tree / ADR / diagram / docs 全て不要判定）のため本実装確定セクションのみ。`overview.md` 更新なし。
