# v01-storage-maintenance-completion Implementation Packet

## Goal

ADR 0012で要求されたstorage maintenanceの未実装部分を完成させる。single-writerの定期maintenanceにthreshold-triggered・page-budgeted `incremental_vacuum` を追加し、起動時/定期にtemp storage使用量を検査してthreshold超過をtyped diagnosticsとして通知する。

## Why

issue #3 / `v01-session-storage` のv0.1 inspectで、ADR 0012のmaintenance要件が部分実装に留まるmedium gapが判明した。`crates/storage/src/writer.rs:222-238,291-335` はPASSIVE checkpointとWAL TRUNCATE、DB合計検査を行うがvacuumを実行しない。`crates/storage/src/db.rs:83-105` のsize検査はDB/WAL/SHMだけでtempを測らず、`config.rs:21-66` にtemp threshold/vacuum budgetもない。ADR 0012 (`0012-metrics-architecture.md:21-29`) がCodexのfreelist肥大とtemp事故への対策として明示した二機能をv0.1.1で実装する。

## Scope

- SQLiteの `auto_vacuum=INCREMENTAL` 前提を新規/既存DBで安全に成立させる。既存DBにVACUUMが必要な場合は起動時無制限実行を避け、明示migration/maintenance手順とtestで互換性を示す。
- `StorageConfig` / `HardLimits` に、vacuum enable/trigger（freelist pagesまたはratio）、1 tick page budget、temp path/threshold、diagnostic再通知方針に必要な最小設定を追加する。
- 定期checkpointと同じsingle-writer maintenance loopで、trigger成立時だけ `PRAGMA incremental_vacuum(N)` を実行する。Nは設定budgetを越えず、一回で全freelistを掃除しない。
- 実行前後のfreelist/page countと実行成否をtracingへ出す。pathやcontentなどsecretになり得る値は不要に出さない。
- temp storageの測定定義を明確にする。SQLite temp directory/管理対象temp pathのfile bytesを測り、起動時とmaintenance tickでthreshold評価する。測れない場合の診断/error policyを決めてtestする。
- threshold超過/復帰をtyped diagnostics eventまたは既存diagnostics sinkへemitし、状態遷移時だけ通知してstormを防ぐ。現行 `FaultEvent` は `SubscriberLagged` のみ (`crates/event-bus/src/event.rs:280-291`) なので、event-busへ最小variantを追加する場合はserde回帰testも更新する。
- 一時SQLite DBでdelete後freelistを作り、複数tickでbudget内回収されるtestを追加する。temp fixtureを低thresholdにしてwarning eventを捕捉し、復帰/重複抑制も検証する。

## Out of scope

- retention policy自体の再設計や自動delete対象の追加。
- downsampling、raw usage guard、schema entityの変更。
- OS全体 `/tmp` の監視や他processのtemp容量管理。evorch/SQLiteが管理するtemp範囲だけを対象にする。
- full VACUUMの定期実行、起動時の無制限blocking maintenance。
- GUI diagnostics panelの表示変更。

## Verification

- 新規DB/既存DBのauto_vacuum互換test。
- freelist fixtureでtrigger未満はno-op、trigger以上は1 tick budget以内、複数tickで段階回収を検証する。
- temp threshold未満/超過/復帰、起動時検査、重複抑制、typed diagnostic payloadを検証する。
- PASSIVE/TRUNCATE checkpoint、hard limit、usage flush/event appendの既存testsを回帰実行する。
- `cargo test -p storage`、event-bus変更時は `cargo test -p event-bus`、`cargo clippy -p storage --all-targets -- -D warnings`、`cargo fmt --all --check`、`git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: storage-memoryのADR 0012 maintenance要件を完成。新規node不要。
- ADR candidate: なし。要件はADR 0012で既決。
- Diagram candidate: なし。
- Docs update: storage-memory overviewへtrigger/budget/temp測定/diagnostic実装値を追記（必須）。
- Closeout learning: auto_vacuum互換方針、budget、threshold、state transitionをwrite back。`write_back_required: true`。
- Guide reachability (G645): 内部maintenance/diagnosticsのみでrole-facing surfaceなし。`no_role_facing_surface: true`。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
