## Goal

single-writer maintenanceへ予算付き `incremental_vacuum` とtemp storage容量診断を追加し、ADR 0012の未実装要件を完成する。

## Why This Slice Exists Now

issue #3のv0.1 inspectで、checkpoint/DB-WAL検査はあるがfreelist回収とtemp検査がないmedium gapが見つかった。Codex事例を踏まえADR 0012が要求した保守境界をv0.1.1で閉じる。

## Current Observed State

- `crates/storage/src/writer.rs:222-238,291-335` は定期PASSIVE checkpoint、WAL超過時TRUNCATE、DB合計warning/suspendを実装する。
- 同writerに `incremental_vacuum` 呼出しはない。
- `crates/storage/src/db.rs:83-105` はdb/-wal/-shmだけを測りtempを含めない。
- `crates/storage/src/config.rs:21-66` にvacuum budget/trigger/temp thresholdがない。
- 現行 `FaultEvent` はSubscriberLaggedだけ (`event-bus/src/event.rs:280-291`)。

## Accepted Baseline You May Assume

- ADR 0012はretention後のbudgeted incremental vacuumと起動時DB/WAL/temp検査を要求する。
- rusqlite 0.40.2 bundled、tracing 0.1、Rust 1.97。
- 既存single-writer maintenance loopとcheckpoint testsを拡張する。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/storage/src/config.rs`, `crates/storage/src/db.rs`, `crates/storage/src/writer.rs`, `crates/storage/src/error.rs`, `crates/storage/tests/`, 必要時 `crates/event-bus/src/event.rs`, `intents/evorch/features/storage-memory/overview.md`

Target part: incremental vacuumとtemp threshold diagnostics

## In Scope

- auto_vacuum incrementalの新規/既存DB互換方針。
- threshold-triggered、1 tick page-budgeted incremental_vacuum。
- 起動時/定期の管理対象temp bytes測定。
- threshold超過/復帰のtyped diagnosticと重複抑制。
- freelist/temp fixture tests。
- overviewへの実装値writeback。

## Out Of Scope

- retention/downsampling redesign、raw usage変更。
- OS全体temp監視、full VACUUM定期実行。
- GUI表示変更。

## Standalone Child Issue Contract

storage single-writerのmaintenance loopに、設定可能なfreelist thresholdと1 tick page budgetを持つ `PRAGMA incremental_vacuum(N)` を追加する。新規/既存DBのauto_vacuum互換性を安全に扱い、起動時と定期tickでevorch/SQLite管理対象temp storage bytesを測定し、設定thresholdの超過/復帰をtyped diagnosticsとして状態遷移時だけemitする。一時DBでbudgeted段階回収とtemp threshold診断をtestし、既存checkpoint/hard-limit/event/metricsを回帰させない。実装値をstorage-memory overviewへ記録する。

## Acceptance Criteria

- incremental vacuum可能なDB設定/移行が新規・既存DBで安全に成立する。
- trigger成立時だけpage budget以内でvacuumする。
- maintenanceは無制限blockingせず前後page情報を診断する。
- 起動時/定期にtemp bytesを測りthreshold超過をtyped emitする。
- threshold未満no-warning、超過/復帰の重複stormなし。
- freelist budget testとtemp threshold testがある。
- 既存checkpoint/hard limits/event/metricsが回帰しない。
- overviewへ実装値を記録する。

## Verification

- `cargo test -p storage`、event-bus変更時は `cargo test -p event-bus`。
- `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Related Links

- [storage-memory/overview.md](../../../intents/evorch/features/storage-memory/overview.md)
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md)
- [0018-sqlite-storage-schema.md](../../../intents/evorch/decisions/0018-sqlite-storage-schema.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice.

- Intent placement: 既存storage-memory。
- ADR candidate: なし（ADR 0012で既決）。
- Diagram candidate: なし。
- Docs update: storage-memory overview（必須）。
- Closeout writeback expected: yes（auto_vacuum、trigger、budget、temp測定、diagnostic state transition）。

## Guide Reachability (G645)

内部maintenance/diagnosticsのみでrole-facing surfaceは追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
