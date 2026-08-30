## Goal

SQLite mutationをstorage専用single-writerへ型で閉じ込め、外部crateにはConnectionやrepo mutationではなく明示的なread-only APIとtyped writer handleだけを公開する。

## Why This Slice Exists Now

issue #3のv0.1 inspectで、ADR 0018のsingle-writer決定がdoc commentだけで、public Rust APIとして強制されていないgapが見つかった。複数Connectionからのwriteを許す公開形状はWAL/flush/accounting invariantを破れるため、v0.1.1で所有権境界を固定する。

## Current Observed State

- `crates/storage/src/lib.rs:10-17` は `db` / `repo` 等をpublic moduleとして公開する。
- `crates/storage/src/repo/mod.rs:1-12` はsingle-writer管理下のみと記すが、全repo moduleがpubでcreate/update/delete/append/upsert関数へ到達可能。
- `crates/storage/src/db.rs:16-29` の内蔵Connectionはcrate-privateだが、外部側で別Connectionを開いてpublic repoに渡すことは型上禁止されない。
- writerは `crates/storage/src/writer.rs:25-79` で専用threadとclone可能command handleを既に持つ。

## Accepted Baseline You May Assume

- ADR 0018: rusqlite Connectionは専用OS threadだけが所有する。
- Rust 1.97 / edition 2024、rusqlite 0.40.2 bundled、serde 1、tracing 0.1。
- v01-storage-metrics-ingress-guardが先にraw UsageEvent guardを同じwriter/event周辺へ追加する。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/storage/src/lib.rs`, `crates/storage/src/db.rs`, `crates/storage/src/repo/`, `crates/storage/src/writer.rs`, `crates/storage/tests/`

Target part: 公開read APIとsingle-writer mutation capability

## In Scope

- repo/db mutation internalsのcrate-private化またはsealed capability化。
- writer threadだけがConnection/write tokenを所有する型構造。
- event/metrics/catalog/entity mutationのtyped command化。
- Connection非公開のread-only facade。
- 外部mutation不可のcompile-fail testとruntime回帰test。
- 既存consumerを調査した最小限のAPI移行。

## Out Of Scope

- schema/migration/WAL/retention/downsampling/projection semantics変更。
- raw usage guard自体（先行 `v01-storage-metrics-ingress-guard`）。merge順序に注意し、そのguardを保持する。
- provider/config/GUIの機能追加。

## Standalone Child Issue Contract

`storage::repo` のmutation関数とSQLite Connectionを外部crateから利用不能にし、`Storage` の専用writer threadだけが書込み所有権を持つ型境界を実装する。clone可能なhandleはtyped command送信のみ、必要なsession/event/entity取得はConnectionを露出しないread-only APIで提供する。event、downsampled metrics、catalog/entityの正規mutationをwriter経由で維持し、外部mutation不可をcompile-fail、writer mutation/read pathをruntime testで証明する。先行raw usage guardをvisibility変更で失わないこと。

## Acceptance Criteria

- 外部crateからrepo mutation関数へ到達できない。
- writerだけがConnection/write capabilityを所有する。
- clone handleからConnectionやrepo権限を取得できない。
- 必要なread pathがread-only public APIで利用できる。
- 正規event/metrics/catalog/entity mutationがwriter経由で成功する。
- compile-fail testが外部mutation不可を固定する。
- runtime testsがread/write回帰を固定する。

## Verification

- compile-fail suite + `cargo test -p storage`。
- 影響consumerのfocused tests。
- `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Related Links

- [storage-memory/overview.md](../../../intents/evorch/features/storage-memory/overview.md)
- [0018-sqlite-storage-schema.md](../../../intents/evorch/decisions/0018-sqlite-storage-schema.md)
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice.

- Intent placement: 既存storage-memory node。
- ADR candidate: なし（ADR 0018の実装）。
- Diagram candidate: なし。
- Docs update: なし。
- Closeout writeback expected: no。採用境界とcompile-fail evidenceをcloseoutへ記録する。

## Guide Reachability (G645)

内部crate API境界のみでrole-facing surfaceは追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
