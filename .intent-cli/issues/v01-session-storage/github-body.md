## Goal

`crates/storage/` に SQLite ベースの event-sourced session 永続化を実装する。sessions / tasks / messages / agent_runs / events のテーブルと migration、ADR 0012 に基づく downsampled metrics（1分バケット per provider/model）と WAL 運用、ADR 0008 に基づく credential 非永続化を提供する。

## Why This Slice Exists Now

storage-memory feature の受け入れ基準は「全 event が SQLite に追記され、session 中断後に resume できること」。v0.1 成功基準も「session が SQLite に永続化される」こと。architecture.md の Storage は「SQLite」指定のみでアクセス層が未確定であり、ADR 0012（downsampled / WAL 運用）と ADR 0008（credential 隔離）を実装に落とす必要がある。後続 slice の前提となるため、v0.1 の早い段階で schema と運用を固定する。

## Current Observed State

- `crates/storage/` は骨格のみ（`Cargo.toml` + `src/lib.rs`、実装なし）。
- SQLite アクセス層（rusqlite / sqlx 等）の選択とスキーマ詳細は未確定。
- storage-memory/overview.md に Open questions「event log のスキーマ詳細（messages と tool_calls の正規化方法）」が残る。

## Accepted Baseline You May Assume

- v01-scaffold 完了（workspace ビルド可）。v01-event-stream 完了（event schema / ring buffer / in-memory 集計の土台あり）。
- ADR 0012: single-writer + バッチ flush、downsampled のみ永続化、WAL 運用（synchronous=NORMAL / wal_autocheckpoint / 定期 PASSIVE checkpoint / ハード上限 / 起動時安全検査 / 自己参照防止）。
- ADR 0008: credential 隔離（keychain 優先 / 0600 fallback）。storage に credential を永続化しない。
- architecture.md の技術スタックは Storage = SQLite。アクセス層の選択はこの slice で確定する（本 packet は **rusqlite** を採用）。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/storage/`

Target part: session / task / message / agent_run / event の永続化

## In Scope

- rusqlite ベースの SQLite アクセス層（single-writer タスクが唯一の書き込み経路）。
- version 管理された migration（起動時適用）。
- テーブル: sessions / tasks / messages / agent_runs / events（event-sourced log）/ downsampled_metrics（1分バケット per provider/model）。
- session / task / message / agent_run / event の CRUD。
- events からの session 復元（state projection、再起動後 resume）。
- ADR 0012 の運用: PRAGMA（WAL / synchronous=NORMAL / wal_autocheckpoint / 定期 PASSIVE checkpoint）/ ハード上限設定 / 起動時サイズ検査 / 自己参照防止の設計。
- metrics 書込経路: event-bus の集計スナップショット → single-writer が downsampled のみバッチ flush。
- credential 非永続化（型 + テストで保証）。
- ADR 0018（SQLite アクセス層・schema・WAL 運用）生成。

## Out Of Scope

- memory パイプライン（v0.4 想定）。
- provider_health テーブル（v0.2 想定）。
- credential の保存・管理（keychain / 0600 JSON。ADR 0008 により storage の外側）。
- OTLP exporter（任意の追加 sink、後続 slice）。
- GUI からの投影・表示（v01-gui-panes）。

## Standalone Child Issue Contract

`turtton/evorch` の `crates/storage/` に SQLite 永続化層を実装する。**rusqlite** を採用し、single-writer タスクが唯一の書き込み経路を持つ。起動時 migration で sessions / tasks / messages / agent_runs / events / downsampled_metrics のテーブルを作成し、session / task / message / agent_run / event の CRUD と、events から session を再現する projection（プロセス再起動後の resume）を提供する。ADR 0012 に従い、raw 高頻度計測イベントは直接書かず、v01-event-stream の集計スナップショットを single-writer がバッチ flush して downsampled_metrics（1分バケット per provider/model）のみ書き込む経路を実装する。PRAGMA 初期化（WAL / synchronous=NORMAL / wal_autocheckpoint / 定期 PASSIVE checkpoint）とハード上限設定（event / session / WAL / DB サイズ）を適用し、起動時サイズ安全検査を行う。credential は永続化しない（ADR 0008、型とテストで保証）。さらに、決定を `intents/evorch/decisions/0018-sqlite-storage-schema.md` として書き、`intents/evorch/features/storage-memory/overview.md` の「event log のスキーマ詳細」Open question を解決済みに更新する。

## Acceptance Criteria

- migration 適用で sessions / tasks / messages / agent_runs / events / downsampled_metrics が作成される。
- session / task / message / agent_run / event の CRUD が動く。
- 再起動後に session が復元できる（event log → projection）。
- WAL 運用（synchronous=NORMAL / wal_autocheckpoint / 定期 PASSIVE checkpoint）とハード上限が PRAGMA 初期化・設定として実装される。
- raw 高頻度イベントの直接永続化をせず、single-writer + バッチ flush で downsampled のみ書く経路がある。
- downsampled_metrics（1分バケット per provider/model）の定義がある。
- credential が storage に書き込まれない（ADR 0008、型 / テストで保証）。
- SQLite アクセス層と WAL / schema 決定が ADR 0018 として host に記録される。

## Verification

- `cargo test -p storage`: migration / CRUD / resume復元 / downsampled flush 経路 / ハード上限拒否 / credential 書込不可。
- `cargo clippy -p storage -- -D warnings` / `cargo fmt --check` / `git diff --check`。

## Related Links

- [storage-memory/overview.md](../../../intents/evorch/features/storage-memory/overview.md) — event-sourced 方針・entity 群
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md) — downsampled / WAL 運用 / single-writer
- [0008-threat-model-phased-adoption.md](../../../intents/evorch/decisions/0008-threat-model-phased-adoption.md) — credential 隔離
- [architecture.md](../../../intents/evorch/technology/architecture.md) — Storage 位置・技術スタック

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: 既存の storage-memory feature を具体化。新規 intent node 不要。
- ADR candidate: `intents/evorch/decisions/0018-sqlite-storage-schema.md`「SQLite アクセス層（rusqlite・single-writer）と storage schema / WAL 運用を確定する」（必須）
- Diagram candidate: なし
- Docs update: `intents/evorch/features/storage-memory/overview.md` の Open questions 更新
- Closeout writeback expected: yes（ADR 0018 生成 + overview.md 更新）

## Guide Reachability (G645)

本 slice は内部 crate のみを追加し、guide 等の role-facing surface は追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.