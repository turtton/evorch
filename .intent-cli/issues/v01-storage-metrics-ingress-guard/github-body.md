## Goal

storage ingressでraw `UsageEvent`の直接永続化を拒否し、downsampled `UsageBucket`だけがsingle-writer経由でSQLiteへ保存される境界を実装する。

## Why This Slice Exists Now

issue #3 / `v01-session-storage` のv0.1 inspectで、ADR 0012のraw非永続化要件がAPIで強制されていないgapが見つかった。高頻度raw eventの再保存を許す状態はADR 0012が避けるべき根本原因そのものであり、v0.1.1 fix roundで先に入口を閉じる。

## Current Observed State

- `crates/storage/src/writer.rs:81-97` の `StorageHandle::append_event` は任意の `event_bus::Event` を受ける。
- `crates/storage/src/repo/event.rs:34-45,194-202` は `EventKind::Usage` をserializeし、他eventと同じ `events` tableへINSERTできる。
- 一方、正規downsample経路は `writer.rs:118-132` の `UsageSink::submit` と `writer.rs:241-246` の `UsageBucket` batch flushとして既に存在する。

## Accepted Baseline You May Assume

- ADR 0012: rawはbounded ring bufferだけに保持し、単一writerがdownsampled集計値のみbatch保存する (`0012-metrics-architecture.md:21-29`)。
- storage schemaとrusqlite single-writerはADR 0018 / issue #3で確定済み。
- workspace baselineはRust 1.97、rusqlite 0.40.2 bundled、serde/serde_json 1、tracing 0.1。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/storage/src/writer.rs`, `crates/storage/src/repo/event.rs`, `crates/storage/tests/`

Target part: raw usage event と downsampled metrics のstorage ingress境界

## In Scope

- public writer handleで `EventKind::Usage` を明示的errorとして拒否。
- repo event appendでも同じinvariantを検査し、内部迂回を防止。
- 拒否時にevents行・accounting・session bytesを変更しない。
- UsageBucket → UsageSink → batch flush経路を維持。
- 非usage eventのappend/read/projection回帰を維持。
- focused unit/integration tests。

## Out Of Scope

- downsamplingアルゴリズム、bucket schema、retention、WAL設定の変更。
- event語彙全体の再設計。
- repo visibility / single-writer型境界（`v01-storage-writer-boundary`）。同じファイル周辺に触れるため本sliceを先にmergeし、後続でguardを保持する。

## Standalone Child Issue Contract

`StorageHandle::append_event` と `repo::event::append_event` の両方で `EventKind::Usage` をSQLite書込み前に識別可能なstorage errorとして拒否し、events table・event accounting・session total bytesを変更しないこと。usage永続化は既存の `UsageSink::submit` が受けるdownsampled `UsageBucket`をsingle-writerがbatch flushする経路だけにする。Lifecycle / Message / Tool / Provider / Fault eventの既存append/read/projectionは維持し、直接raw拒否とdownsampled成功をテストで証明する。

## Acceptance Criteria

- `StorageHandle::append_event(EventKind::Usage)` はINSERT前に拒否される。
- `repo::event::append_event(EventKind::Usage)` も拒否される。
- 拒否時にevents行数・accounting・session total bytesが変化しない。
- raw usageをeventsへ保存する別公開APIを追加しない。
- UsageBucketはbatch flush後にdownsampled_metricsへ保存される。
- 非usage eventのappend/read/projectionが回帰しない。

## Verification

- `cargo test -p storage`（handle/repo拒否、DB不変、usage flush、非usage回帰）。
- `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Related Links

- [storage-memory/overview.md](../../../intents/evorch/features/storage-memory/overview.md)
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md)
- [0018-sqlite-storage-schema.md](../../../intents/evorch/decisions/0018-sqlite-storage-schema.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice.

- Intent placement: 既存storage-memory nodeを具体化。新規nodeなし。
- ADR candidate: なし（ADR 0012 / 0018の実装gap修正）。
- Diagram candidate: なし。
- Docs update: なし（既存文書が正しい要件を記載済み）。
- Closeout writeback expected: no。guard位置とテスト結果はcloseout evidenceへ記録する。

## Guide Reachability (G645)

内部storage APIの修正のみでrole-facing surfaceは追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
