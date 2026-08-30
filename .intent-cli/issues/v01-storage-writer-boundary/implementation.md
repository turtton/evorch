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
