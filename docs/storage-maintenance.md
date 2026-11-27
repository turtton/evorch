# ストレージ保守手順

イベント DB（`evorch-events.db`）を縮小・整備する `evorch maintenance` の
`convert-bodies`、`compact-events`、`backfill-events`、`vacuum` の手順書。コマンド一覧は
`evorch maintenance` の USAGE（`crates/evorch/src/maintenance.rs`）が正本。
永続化されるイベントの種別と保持方針は
[io-performance.md](io-performance.md#イベントの永続化と保持) を、タスク進捗の
保守は [task-progress-maintenance.md](task-progress-maintenance.md) を参照。

## 使い分け

- `evorch maintenance backfill-events` は、既存イベントの `subkind` / `run_id` 列だけを埋める軽量コマンド。GUI/runtime の稼働中にも使用できる。DB の物理サイズ縮小や delta の集約は行わない。
- `evorch maintenance compact-events` は、既定で読み取り専用 preview。実際の変換には `--apply` が必要。preview と apply の両方で、対象 DB を使うすべての GUI/runtime/reader を停止する。
- `evorch maintenance vacuum` は、旧 DB の `auto_vacuum` を INCREMENTAL に一度だけ切り替える軽量コマンド（[auto_vacuum の一回限りの移行](#auto_vacuum-の一回限りの移行)）。イベントの変換は行わない。
- `evorch maintenance convert-bodies` は、schema 33〜43 の旧形式 DB を一度だけ現行形式（schema 46）へ変換する。既定は読み取り専用 preview で、`--apply` で変換する。変換が済むまで、旧形式の DB は他のコマンドでも GUI でも開けない（[convert-bodies による変換](#convert-bodies-による変換)）。
- いずれも runtime やモデル呼び出しを開始せず、暗黙の schema migration は行わない（`convert-bodies` だけが、変換の一部として migration を明示的に適用する）。compact・backfill・`strip-task-progress-cursor` は、現行の形式（schema 44。会話は `run_messages`、provenance は `run_message_provenance`、events の大きな本文は token）の DB だけを受け付ける。それより古い形式の DB は `ConversionRequired` で拒否し、何も変更しない（`vacuum` はファイルの再構築だけで会話を解釈しないため対象外）。旧形式の DB は、すべての GUI/runtime を停止したうえで、[`evorch maintenance convert-bodies`](#convert-bodies-による変換) で変換する。

以下の `$DB` は対象を確認した DB の絶対パスに置き換える。既定パスはユーザー設定ディレクトリ内の `evorch-events.db`。

## 本文の保存方式（概要）

- **本文の正本は `run_messages` と `content_blobs`。** run の会話は `run_contexts`（header）と、メッセージ 1 件ごとの `run_messages` 行に保存する。UTF-8 で 2 KiB（2,048 バイト）以上の文字列は、内容アドレスの `content_blobs` に置いて骨組みには token だけを残す。blob は zstd レベル 3 で圧縮し、縮む場合だけ圧縮形式を採用する（しきい値は `body_ref.rs` の `TOKEN_THRESHOLD_BYTES`、圧縮は `body_store.rs` の `ZSTD_LEVEL`）。保存は変更のあったメッセージだけを書き（差分保存）、保存直後の全量読み戻しは行わない。詳細は[会話の保存形式](#会話の保存形式run_messages)。
- **provenance は 1 メッセージ 1 行。** 以前は run ごとに `run_context_provenance.map_json` の 1 列へ全体を保存していた。現在は header と `run_message_provenance` の行に分割する。
- **events の大きな本文も token 化されている。** Tool / Message / compaction の events の payload のうち、2 KiB 以上の文字列を同じ `content_blobs` に置く（`events.body_format`）。読み出しは token を本文に戻す。`evorch inspect events --bodies refs` は blob を読まずに token の hash と長さだけを示す。
- **GUI は本文を遅延して読み込む。** 保存済みの本文はペインに表示されるまで参照のまま保持する。読み込み中はスピナーとサイズ、失敗時は "Couldn't load content" と Retry / Copy details を出す。busy や一時的な失敗は自動で再試行する（最大 3 回。`AUTO_RETRIES`）。missing / corrupt の整合性エラーは自動では再試行せず、1 回だけ報告する。読めない stored event は transcript 上で前後の位置に gap として表示する。読み出しは専用スレッドで行い、1 バッチは最大 256 本・16 MiB、キャッシュ（LRU）は 64 MiB（`crates/gui/src/body/resolver.rs`）。
- **旧形式は開かない。** schema 44 未満の DB は、`Database::open`・GUI の起動・`evorch inspect`・保守コマンドのいずれでも `ConversionRequired` で拒否され、DB は変更されない（[回復手順](#回復手順)）。

## Tool 本文の保持期限とオンライン回収

auto archive と同じ既定 7 日（設定 [storage.thread_retention] tool_body_delete_after_days、
0 は本文除去だけを無効化）。1 週間以上更新のない本文の参照は少なく、必要な thread は
pin / eval sample で守れるため、この分かりやすい値にそろえている。

既存の 5 秒 retention tick に最大 1 unit を追加し、canonical thread の root・子 run
全体の活動（入力・メッセージ・turn・Running 遷移）を再確認する。Tool イベント自身は
活動に数えず、archive の有無を問わない。実行中、すべての入力待ち、未確定 admission、
未回答質問（root / link 含む）、pin、eval sample、attached / summary の open lane は
保護する。detached lane、未完了 goal、旧 family budget hold だけでは保護しない。
所属不明・backfill 未完了・破損・1,024 run 超過は無変更で保留する。

対象は body_format=1 の ToolStarted.input と ToolCompleted.output/detail
内の、宣言長 2,048 UTF-8 bytes 以上の有効な blob token だけ。
[removed by retention] と表示し、名前・ID・状態・成否、JSON 構造・短い値は残す。
**run_messages、run_contexts、checkpoint（provider 履歴）は変わらず、再開・cache prefix は保持する。**
他のイベントや provider 履歴等と共有される blob は消えず、完全な情報消去にはならない。

1 unit は最大 256 event headers、保存時 / 展開後 skeleton 各 2 MiB、旧 / 新参照各 2,000 件。
blob 本体は展開しない。payload と参照を同じ IMMEDIATE transaction で更新し、unit 間は
writer queue を解放する。参照が外れた blob は既存 trigger から GC queue へ進む。
既存 maintenance は既定 60 秒ごとに GC（256 候補・8 MiB / tick、単体 4 MiB 以下）と
incremental vacuum（256 free-list pages / tick、開始閾値 1,024 pages）、checkpoint を行う。
pointer-map の不要ページも同時に返る場合がある。自動 full VACUUM は行わず、閾値未満の
残余を必ずゼロにするものでもない。NONE の DB は下記の一回限りの移行が必要。

消化目安（連続稼働・4 KiB/page、上限の単純計算、保証ではない）:

- GC 241,521 候補は約 944 tick / **15.7 時間**（64 候補なら約 62.9 時間）。共有・再投入・byte budget で延びる。
- 既存 free-list 191,441 pages（約 748 MiB）は **12.5 時間**、新規約 279 MiB 相当を加えると **17.1 時間**。
- 参照解除は変更 unit 数 × 5 秒に候補探索が加わる。仮に 4,229 unit なら約 5.9 時間。
  3 段階は並行するが、完全直列なら約 38.7 時間の見積もり。履歴索引も変更 unit ごとに最大 1 再構築を要する。

これらは診断で得た候補量（保護除外前）の上限側見積もりで、約 279 MiB の削減保証ではない。
停止時間・長い reader・対象外 token・履歴索引の追加書込みも実際の消化時間に影響する。
GC の合成 WAL DB 計測は 64 候補 2.415 ms / WAL +90,672 bytes、256 候補 3.795 ms /
+123,632 bytes（各 1 回、環境依存）。採用条件 50 ms / 64 MiB 以下を満たしたため 256 を採用。
vacuum の予算・閾値は変更していない。

### 既に INCREMENTAL の DB を停止中に full VACUUM する

「evorch maintenance vacuum」は既に INCREMENTAL なら何もしないので、**full VACUUM の代用ではない**。
必要な場合だけ以下を手動で行う。

1. 全 core・GUI・reader と自動再起動を停止する。他プロセスが DB を開かないことを確認する。
2. DB、バックアップ先、SQLite temp 領域（SQLITE_TMPDIR / TMPDIR）の空き容量を確認する。
   同一 volume なら DB サイズの **4 倍以上**を保守的な目安にする。別 volume も個別に確認する。
3. DB と **存在しない安全な絶対パス** BACKUP を指定する（例は単一引用符を含まないパス）。
   元の本文を含むバックアップを安全に保管する。下記を一段ずつ実行し、各エラー、busy、
   integrity の異常で止める。checkpoint の先頭列は 0、integrity は ok を確認する。

~~~sh
umask 077
test ! -e "$BACKUP" || exit 1
sqlite3 -bail "$DB" ".backup '$BACKUP'"
sqlite3 -bail -readonly "$BACKUP" 'PRAGMA integrity_check;'

sqlite3 -bail "$DB" 'PRAGMA busy_timeout=0; PRAGMA wal_checkpoint(TRUNCATE);'
# checkpoint の結果の先頭列が 0 でなければ、ここで止める。
sqlite3 -bail "$DB" 'PRAGMA busy_timeout=0; VACUUM;'
sqlite3 -bail "$DB" 'PRAGMA busy_timeout=0; PRAGMA wal_checkpoint(TRUNCATE);'
# 再び checkpoint の先頭列 0 を確認してから検査する。
sqlite3 -bail "$DB" 'PRAGMA integrity_check; PRAGMA auto_vacuum;'
~~~

4. integrity が ok、auto_vacuum が 2（INCREMENTAL）、保持データが読めることとサイズを確認してから core を再開する。
   WAL / SHM / maintenance lock を手で消さない。失敗時は再開せずバックアップを保持する。

同梱 SQLite の合成 INCREMENTAL / WAL DB で、SQLite backup API（CLI の .backup と同じ経路）、
上記 SQL、バックアップの free-list 保持、元 DB の縮小、保持データ・integrity・mode を試験している。

## 保守コマンドの早見表

| 目的 | コマンド | 節 |
| --- | --- | --- |
| 容量の調査（読み取り専用。稼働中も可） | `evorch inspect [--db <path>] storage [--physical] [--top <n>]` | [容量の確認](#容量の確認evorch-inspect-storage) |
| 旧形式 DB の変換（停止中のみ） | `evorch maintenance convert-bodies [--apply] [--backup <new-path>] [--event-batch-rows <1..100000>]` | [convert-bodies](#convert-bodies-による変換) |
| `auto_vacuum` の一回限りの切替（停止中のみ） | `evorch maintenance vacuum [--apply] [--backup <new-path>]` | [auto_vacuum](#auto_vacuum-の一回限りの移行) |
| events の縮小（停止中のみ） | `evorch maintenance compact-events …` | [オフライン縮小](#オフライン縮小preview--必須バックアップ--apply) |
| archive を妨げる未回答質問と open lane の一回限りの整理（停止中のみ） | `evorch maintenance cleanup-archive-blockers [--config-dir <dir>] [--db <path>]`（preview）／`… --apply --plan <preview.json> --backup <path>` | [archive を妨げるものの整理](#archive-を妨げるものの整理cleanup-archive-blockers) |
| アーカイブ済みスレッドの期限削除 | 設定 `[storage.thread_retention] archived_delete_after_days`（既定 7、0 で新規の確定を無効化） | [runtime](#スレッドの期限削除runtime起動フェーズ束縛回収)、[GUI](#スレッドの期限削除gui起動表示採番) |

期限削除に専用のコマンドはない。設定の既定値は 7 日で、GUI を起動すると、起動の排他フェーズ（他の evorch プロセスが動いていない起動だけ）が期限の来たアーカイブ済み family を確定し、以後は背景の回収が DB の行を少しずつ消す。`archived_delete_after_days = 0` にすると新規の確定は行われない（回収や巨大行の保守は続く）。時刻を持たない古いアーカイブは日数に関係なく直ちに対象になり、pin・評価サンプル・実行中の family は除外される。確定した削除は取り消せない。GUI での起動手順・表示（アーカイブ一覧のホバーに "Deletes in N days" など）・通知は[GUI の節](#スレッドの期限削除gui起動表示採番)を参照。

## convert-bodies による変換

`evorch maintenance convert-bodies` は、旧形式の DB を一度だけ現行形式（schema 46）へ変換するオフライン専用コマンド。変換元は schema 33〜43 の DB で、成功すると 46 になる（migration 46 は ADR 0043 の PR 連携テーブルの追加だけで、本文の変換はない）。runtime やモデル呼び出しは開始しない。

| 変換元 | 変換する内容 |
| --- | --- |
| v33〜v39 | migration 34〜39 を（バックアップ後に）先に適用し、会話本文を `run_messages` / `content_blobs` へ、provenance を行へ、events の大きな本文を token へ変換し、旧列（`run_contexts.messages_json` など）を削除する |
| v40〜v42 | 会話本文は変換済み。**provenance と events** を変換する（USAGE によると v41・v42 は lane ledger と memory favorite の追加だけの差） |
| v43 | provenance も変換済み。**events だけ**を変換する |
| v44 | 会話・provenance・events は変換済み。変換は行わず、`already_converted: true` で何も書かずに終わる。migration 45（lane の列）と 46（PR 連携テーブル）は通常の起動が適用する |

v33 未満の DB と、現行（46）より新しい DB は変換できず拒否される。

### 手順

1. すべての evorch GUI とすべての evorch プロセス（headless・reader を含む）を停止する。
2. preview（既定。読み取り専用）を実行する。

   ```sh
   evorch maintenance convert-bodies --db "$DB"
   ```

   preview は、DB の隣に保守リース `<db>.maintenance-lock` が既にあること、`-wal` が空であることを要求する。リースが無い DB（リース導入前のビルドしか動かしていない DB や、変換用に作ったコピー）では、空ファイル（mode 0600）を先に作るよう案内するエラーになる。`--apply` はリースを自動で作る。非空の WAL は preview でも apply でも必ず拒否する。
   JSON の `source_schema` / `target_schema`、`source_runs`、`required_free_space_bytes` と `space_requirements`（DB・バックアップ先・SQLite の一時領域の各 volume に必要な空き）、`estimated_growth_bytes`、`analysis`（変換できない run・event、サイズ超過、回収が終わらない原因など）を確認する。変換できないデータがあると message は "preview found data that cannot be converted; --apply would refuse" になり、apply も何も変更せずに拒否する。
3. 空き容量を用意する。DB サイズの 4 倍（バックアップと VACUUM の分）が目安で、volume ごとの必要量は `space_requirements` にある。SQLite の一時領域は `SQLITE_TMPDIR`、`TMPDIR`、`/var/tmp` の順で選ばれるので、空きの小さい volume なら空きのある場所へ向ける。
4. apply する。

   ```sh
   evorch maintenance convert-bodies --db "$DB" \
     --backup "/secure/path/evorch-before-convert-bodies.db" --apply
   ```

   events は 1 transaction あたり 1,000 行（`--event-batch-rows` で 1〜100,000 を指定。ただし 1 transaction のデコード済み payload は 16 MiB まで）ずつ変換する。バックアップ・run ごとの検証・VACUUM に、数分以上かかりうる。
5. 成功 JSON の `integrity_check: "ok"`、`foreign_key_check: "ok"`、`vacuum: true`、`checkpoint_truncated: true`、`auto_vacuum_after: 2`、`mismatched_runs: 0`、`unconverted_runs_remaining: 0` を確認する。`file_bytes_before` / `file_bytes_after` でサイズ変化、`events` で events の変換件数を確認する。GUI を起動して履歴が表示されることを確認してから、バックアップの保存方針を決める。

### バックアップ

- apply は最初の変更の前に、検証済みバックアップ（`VACUUM INTO` → `integrity_check` → fsync）を必ず作る。無効化オプションはない。作成した場所は stderr に `Verified maintenance backup: <path>` として出る（失敗時のレポートにも出る）。
- `--backup <new-path>` を指定した場合はそのパス（既存ファイルの上書きは拒否）。省略時は `<DB のパス>.before-convert-bodies-<unix ナノ秒>.db`、つまり **DB と同じディレクトリ**に作る。
- Unix でバックアップの mode は 0600。元の会話・ツール出力を含むため、安全な場所に保管する。バックアップは、変換した DB を起動して確認し終えるまで消さない。
- 空き容量は事前に検査するが予約はしない。変換中に他の処理が空きを使い切らないようにする。

### 変換が行うこと・拒否すること

- **非空の WAL は拒否する。** WAL の復旧はバックアップ前に DB 本体を書き換えるため、このコマンドは復旧しない。変換前の DB を書いたビルドの evorch を正常終了させ、WAL を checkpoint してから実行する。変換が中断した後に WAL が残っているときは、バックアップから戻す（[回復手順](#回復手順)）。
- **未変換の DB では起動が拒否される（`ConversionRequired`）。** schema 44 未満の DB を `Database::open` や GUI の起動で開くと、DB を変更せずに拒否する。エラーは "stop every evorch process and run the conversion command (`evorch maintenance convert-bodies` …)" と案内する。`evorch inspect` や他の `evorch maintenance` コマンドも同様。
- **tombstone は変換前に retention で回収する。** 確定済みの期限削除（tombstone）の run の行は、変換の前に既存の回収処理（有界の回収と排他フェーズの回収）で DB から消し、変換の対象にしない。回収が終わらない場合はコマンドが止まる。新しい削除対象は選ばず、DB 外のファイルには触れない。結果は JSON の `retention` にある。
- **`canonicalized_runs`。** 復元できない run（`restorable = 0`）は provider へ再送されないため、旧列のバイト列がシリアライザの出力と違っていても、同じ JSON 値であることを確認したうえでシリアライザのキー順で保存する。この run は会話の索引が snapshot hash と合わなくなるため、索引も再生成する。件数と run ID は JSON の `canonicalized_runs.count` / `run_ids` に出る（preview では `analysis.canonicalizable_runs`）。実 DB の変換では 51 件の run が並べ直された。復元できる run のバイト列が違うときは変換せず、停止する。
- **検証。** 各 run は自分の transaction で変換し、通常の reader で読み戻して旧列とバイト単位で比較してから commit する。provenance は型付き map として、events は JSON 値として比較する。最後に旧列を削除し、スキーマを新規 DB と比較し、VACUUM（`auto_vacuum=INCREMENTAL`）・WAL TRUNCATE・`integrity_check` を行ってから `user_version` を 46 にする。公開が最後の書き込みなので、途中で止まった DB は通常の起動で拒否される。
- 変換済みの DB に再実行しても何も書かれない（`already_converted: true`）。

## 計測値

実 DB での計測（運用時の実測値で、コードやテストからは再現できない）。

- **v33 → v40 の変換（会話本文の変換）。** ファイルサイズは +11.5% だった。これは書込量の削減（差分保存）を優先した結果で、サイズの縮小は目的にしていない。
- **events の token 化。** コピーした DB で 2.45 GB → 2.27 GB（約 7% 減）になった。
- **token 化のしきい値。** 2 KiB（UTF-8 で 2,048 バイト以上の文字列）。これより短い文字列は本文に残る。

これらは特定の DB の値で、他の DB のサイズ変化は保証されない。preview の `estimated_growth_bytes` は VACUUM 前の増加の上限の見積もりで、最終サイズではない。縮小の確認は、変換後の `file_bytes_after` と `evorch inspect storage --physical` の `freelist_bytes` で行う。

## 回復手順

### 変換の失敗・中断（`convert-bodies`）

変換に再開（resume）はない。失敗時の stderr のレポートは、失敗した段階（`input` / `lock` / `analyze` / `backup` / `migrate` / `prepare` / `prepare_events` / `retention` / `convert` / `provenance` / `convert_events` / `verify` / `drop_column` / `vacuum` / `publish`）、run、`reason`（`mismatch` / `sqlite` / `io` / `refused`）、変換済みの run 数、検証済みバックアップのパスを示し、末尾が次のどちらかになる。

- **"the database was not changed"。** バックアップより前の失敗（preview の拒否、変換できないデータ、空き容量不足、使用中、非空の WAL など）。DB は変わっていない。原因を取り除いて同じコマンドをやり直す。
- **"the database was changed"。** バックアップ後の失敗（プロセスの kill や停電による中断も同じ扱い）。変わった DB を出発点にせず、見かけが古い schema でも、**変換前に作ったバックアップだけ**から戻す。
  1. すべての evorch プロセスを停止したまま、その DB では起動しない。
  2. DB 本体と `-wal` / `-shm` を、まとめて検査用の安全な場所へ移す。
  3. 検証済みバックアップを DB のパスへコピーする（古い `-wal` / `-shm` と組み合わせない）。
  4. 所有者と mode を確認する。
  5. `convert-bodies` を preview し、新しい `--backup` のパスを指定して `--apply` をやり直す。
  6. 変換した DB を起動して確認し終えるまで、最初のバックアップを保持する。

  `.maintenance-lock` を削除・作り直して排他を迂回しない。

### 起動の拒否（`ConversionRequired`）

GUI・`evorch inspect`・保守コマンドが "the database (schema version N) uses the old conversation storage (…)" で止まるのは、DB が schema 44 未満のとき、または変換が途中で止まった形のとき。このエラーでは DB は変更されない。

- 単に未変換の DB なら、すべての evorch プロセスを停止し、[手順](#手順)で `convert-bodies` を preview → `--apply` する。
- detail が "an interrupted conversion" の DB は、変換の中断の跡なので、上の「the database was changed」の手順でバックアップから戻して変換をやり直す。
- `SchemaTooNew`（DB の schema が、この evorch の対応より新しい）は、より新しい evorch で開く。

### バックアップからの復元

上の手順 1〜4 と同じ。稼働中の DB 本体だけをコピーして「バックアップ」と見なさない。元のファイルや最初のバックアップを上書きで失わないこと。復元した DB は変換前の schema のままなので、再度 `convert-bodies` を行うまで GUI の起動は `ConversionRequired` で拒否される。`compact-events` / `vacuum` のバックアップからの復元も同じ手順（[中断・再実行・復元](#中断再実行復元)）。

## オンラインの列バックフィル

```sh
evorch maintenance backfill-events --db "$DB" --batch-rows 1000 --max-rows 50000
```

JSON の `converted` / `reattributed`（run 帰属のなかった Orchestrator イベントへの `run_id` 補完）/ `undecodable` / `batches` / `remaining` を確認する。`remaining` が 0 になるまで繰り返せる。`--max-rows` を省くと全未処理行を対象とし、0 を指定すると変換せず残数だけを返す。`--batch-rows` は 1～100,000、既定 5,000。

各バッチの commit 後に passive checkpoint を行うが、長時間 reader が古い snapshot を保持すると WAL は増え続けうる。空き容量を監視し、必要なら処理量を小さくする。書き込み競合で失敗した場合も、完了済みバッチは保持され、再実行で残りを処理する。オフライン保守中の DB には共有リースを取得できず実行を拒否する。

## オフライン縮小：preview → 必須バックアップ → apply

1. 対象 DB の絶対パスを確認し、すべての evorch GUI/runtime とその DB の reader を停止する。正常終了により WAL を checkpoint する。非空 WAL がある preview は、読み取り専用を保証するため SQLite を開く前に拒否する。既存の `.maintenance-lock` がない場合も拒否するため、現行 evorch を一度正常起動・正常終了してから行う。
2. preview を実行する。

   ```sh
   evorch maintenance compact-events --db "$DB"
   ```

   `--batch-rows` は 1～100,000、既定 1,000（バックフィルとは既定値が異なる）。`sidebar.json` は GUI と同じユーザー設定ディレクトリから読み取り専用で参照する。別の GUI state を使っている場合や DB コピーを検証する場合は、preview と apply に同じ `--sidebar "/secure/path/sidebar.json"` を指定する。DB 外の送信済みユーザー入力も履歴の表示順に影響するため、その時刻を集約境界として使う。sidebar を特定・読み取り・parse できない場合（64 MiB 上限超過を含む）は、時刻差の推測をせず、delta 集約を無効化して stderr と JSON に警告する。他の削除・codec 圧縮・会計修復は続行できる。`delta_aggregation_enabled` / `sidebar_boundary_count` / `warnings` で状態を確認する。

   `before` / `after` は実在する snapshot なので、preview では同一のまま。変換後の種別別行数は `projected_event_rows` を参照する。各工程の `input_rows` / `output_rows` / payload bytes、圧縮サンプル、`backfill.remaining`、`oversized_rows`、`required_free_space_bytes` を確認する。events の圧縮サンプルは、削除・集約を仮想的に適用した後に生存する平文 payload が対象（小さい delta の集約で 2 KiB を超える値も含む）。events は最大 1,000 値からの推定であり、DB の最終物理サイズを保証しない。会話（`run_messages`）は保存時に圧縮済みなので、compact は書き換えない（JSON の `context_compression` / `compressed_context_values` は廃止した）。
3. DB とバックアップの保存先に、少なくとも preview の `required_free_space_bytes`（元 DB・論理ページ・sidecar サイズを考慮した最大値の 4 倍）の空きを用意する。別 volume のバックアップ先にも保守的な 4 倍検査を行う。検査は容量を予約しないため、保守中に他の処理が空きを使い切らないようにする。
4. 新規のバックアップ先を指定して apply する。

   ```sh
   evorch maintenance compact-events --db "$DB" \
     --backup "/secure/path/evorch-before-compact.db" --apply
   ```

   `--backup` を省くと DB パスに `.before-compact-events-<timestamp>.db` を付加した新規ファイルを使う。既存ファイルの上書きは拒否する。バックアップ無効化オプションはない。ソース変換前に `VACUUM INTO`、`integrity_check`、fsync を完了し、検証済みパスを stderr に出力する。Unix で新規バックアップの mode は 0600。バックアップには元の会話・ツール出力等が含まれるため、安全な場所で保管する。
5. 成功 JSON の `integrity_check: "ok"`、`vacuum: true`、`checkpoint_truncated: true`、`after.unbackfilled_rows: 0`、`auto_vacuum_after: 2` を確認する。種別別件数・ファイルサイズ・`elapsed_seconds` を記録し、GUI の履歴表示を確認してからバックアップの保存方針を決める。

## auto_vacuum の一回限りの移行

新規 DB は `auto_vacuum=INCREMENTAL`（2）で作られ、稼働中の writer が削除後の空きページを少しずつファイルシステムへ返す。それ以前に作られた DB は `auto_vacuum=NONE`（0）のままで、行を削除してもページは DB 内で再利用されるだけでファイルは縮まない。SQLite はモードの変更を full VACUUM でしか反映できないため、起動時には自動で行わない。通常起動は、NONE の DB を開くと警告ログを 1 プロセスにつき 1 回出し、この手順を案内する。

`compact-events --apply` は VACUUM の直前に INCREMENTAL を設定するため、実行すれば移行も完了する（JSON の `auto_vacuum_before` / `auto_vacuum_after`）。すでに compact 済みの DB など、イベント変換が不要な場合は `vacuum` を使う。

1. すべての evorch GUI/runtime と reader を停止する。前提は compact-events と同じ（既存の `.maintenance-lock`、schema が現行以下）。さらに、非空の `-wal` が残っている DB は preview でも apply でも拒否する（異常終了後など）。WAL の内容次第で判定が変わりうる一方、SQLite の復旧はバックアップ前に DB 本体を書き換えるため。evorch を一度起動して正常終了し、WAL を checkpoint してから再実行する。
2. preview（既定、読み取り専用）で現在のモードと必要な空き容量を確認する。

   ```sh
   evorch maintenance vacuum --db "$DB"
   ```

   `auto_vacuum_before` が 0（または 1）なら移行できる。2 なら `already_incremental: true` で、何も行わない。モードの判定は読み取り専用接続だけで行い、preview と、mode=2 に対する `--apply` は DB 本体・`-wal`・`-shm` を作成・変更・削除しない（writable 接続とバックアップは、移行が必要な場合だけ）。
3. DB とバックアップ先に `required_free_space_bytes`（compact-events と同じ、元 DB・論理ページ・sidecar の最大値の 4 倍）の空きを用意する。SQLite の VACUUM は一時ファイルを SQLite の temp ディレクトリ（`TMPDIR` など）にも作るため、DB と別 volume の temp が小さい場合は `TMPDIR` を空きのある場所に向ける。
4. apply する。

   ```sh
   evorch maintenance vacuum --db "$DB" --backup "/secure/path/evorch-before-vacuum.db" --apply
   ```

   compact-events と同じく検証済みバックアップ（`VACUUM INTO` + integrity_check + fsync、`--backup` 省略時は `<DB>.before-vacuum-<timestamp>.db`、バックアップ無効化なし）を作成してから、INCREMENTAL を設定して full VACUUM、WAL TRUNCATE、integrity_check を行う。VACUUM は DB サイズとディスク速度に応じて分〜それ以上かかる。
5. 成功 JSON の `auto_vacuum_after: 2`、`integrity_check: "ok"`、`vacuum: true`、`checkpoint_truncated: true` を確認する。`file_bytes_before` / `file_bytes_after` でサイズ変化を確認し、起動後に警告ログが消えていることを確認する。

失敗や中断で DB の mode が 2 にならなかった場合は、データは未変更（VACUUM は原子的）で、バックアップが残る。同じコマンドを再実行できる。mode が 2 の DB に再実行しても、バックアップや VACUUM は行われない。

## 変換内容と保持契約

apply の順序は固定：

1. `Database::backfill_event_columns` で未バックフィル列を埋める。復号不能行は `subkind=''` として残す。
2. SubscriberLagged と RunProgress を削除する。
3. MessageDelta を run 別・応答境界別に集約する。
4. ReasoningDelta を同じ帰属・境界規則で集約する。
5. Provider の RequestCompleted / FirstTokenObserved / CacheReuseObserved の期限切れ行だけを削除する。
6. 既存 codec と writer と共通の判定で Tool / Message / Compaction payload と run_contexts の平文列を圧縮する。2 KiB 未満、圧縮で縮まない値、AgentMessage、承認等の payload 帰属 Tool は平文のまま。
7. セッション・日次の event bytes と `sessions.total_event_bytes` を再計算する。
8. `PRAGMA auto_vacuum=INCREMENTAL` を設定して VACUUM し、`auto_vacuum` が 2 になったことを検証する。続けて WAL TRUNCATE と integrity_check を行う。

既定は表示内容を維持する。MessageDelta は一律削除しない。現行 GUI reader が最初の delta の位置を完了本文の表示位置として使うため、同じ MessageDelta 形式のまま本文を連結し、先頭 ID / metadata を残す。完了済み応答・未完了応答とも同形式で扱い、reasoning や tool の境界を越えて表示順序を動かさない。MessageCompleted / MessagePart の内容は変更しない。

集約は JSON escape と UTF-8 を考慮した 64 KiB 以下で、元行の境界で区切る。日付・schema・session・run・応答の境界、および処理量を制限する安定した ID window を越えない。子 run の終了通知などが root の thread にも配送されるため、run 帰属だけで境界を推測せず、対象 delta 以外の保存イベントで全 run の集約を区切る（削除する表示対象外の noise・期限切れ Provider 観測は除く）。同種 delta の別 run 同士の交互記録は集約できる。既存の単独 64 KiB 超行は、ID を振り直さず、復号後の本文と ID を維持して `oversized_rows` に報告する。集約本文にも既存 SecretGuard を適用し、秘密を分割によって検査から逃さない。検査失敗時は当該 group の書き換えを行わずエラーになる。

sidebar の履歴時刻を全イベントの再生順に照合し、GUI がユーザー入力を挿入する位置で全 run の集約を区切る。status 等のない legacy 入力を含め、全 `user_messages[].at` を保守的な境界に使い、会話本文は解析結果に保持しない。sidebar 自体は書き換えない。既知の制約：wall_clock が非単調で、削除イベントだけが入力挿入の時刻判定を越える極端な履歴は表示順保持の対象外。

Provider retention の既定は 90 日。`--provider-retention-days N` で変更でき、0 は期限削除を無効化する。履歴復元判定に必要な RequestStarted は常に保持する。preview と apply で設定を合わせ、境界時刻付近の行は実行時刻の進行で対象が変わりうることに注意する。

`--delta-mode delete` は明示的な破壊的オプトイン。ReasoningDelta だけでなく、未完了本文を含む MessageDelta も削除する。reasoning 表示と未完了本文が失われる旨を stderr に警告する。通常の縮小では指定しない。

## 中断・再実行・復元

集約 group は先頭行の更新と後続行の削除を一つの transaction で行い、他の工程もバッチごとに commit する。日次・session 別のトリガ会計に加え、`sessions.total_event_bytes` の差分更新も各書き換えと同一 transaction 内で行うため、後の工程で失敗しても、正常だった会計を途中状態で不整合にしない。最後の再計算では既存の会計ずれも修復する。中断した場合は、完了まで evorch を再起動せず、同じ設定で再実行する。明示したバックアップ先が既に存在するなら、別の新規パスを指定するか既定の時刻付きパスを使う。最初のバックアップは、全工程が成功するまで保持する。部分処理後の新しいバックアップは、その部分処理済み状態を保存する点に注意する。

データ変換は再実行可能・冪等だが、各 apply のバックアップ作成・VACUUM・検査は毎回実行する。「変化なし」は追加の論理変換がない意味であり、書き込みゼロやバックアップ不要の意味ではない。`.maintenance-lock` を削除・作り直して排他を迂回しない。

復元する場合もすべての接続を停止し、現在の DB と WAL/SHM を別の安全な場所へ退避してから、検証済みバックアップを DB 本体パスへコピーする。元ファイルや最初のバックアップを上書きで失わないこと。所有者・アクセス権を確認し、正常性と履歴を確認してから通常運用に戻す。稼働中の DB 本体だけをコピーして「バックアップ」と見なしてはいけない。

## 容量の確認（`evorch inspect storage`）

縮小の前後や肥大化の調査には、読み取り専用の `evorch inspect [--db <path>] storage [--physical] [--top <n>]` を使う。稼働中の DB にも使える。

- **DB 本体は変更しない。** 読み取り専用で開き、接続を閉じるときの checkpoint も無効にしてあるため、非空の WAL が残っていても DB 本体へ書き戻さない。migration もしない。
- **作成され得るファイル。** `-wal` / `-shm` は、他の読み取り側（GUI など）と同様に SQLite が作成・使用する。保守リース `<db>.maintenance-lock`（空）も、まだ無ければ作る。
- **整合性。** 集計全体を 1 つの読み取りトランザクションで行うため、集計中に writer が commit・checkpoint しても、結果は開始時点の 1 つの commit 済みスナップショットになる。
- **GUI の起動を妨げない。** リースは共有で取るので、稼働中の evorch とも、これから起動する evorch とも共存する。
- **オフライン保守とは共存しない。** 保守がリースを排他で持っていると、`database is under offline maintenance` で何も読まずに拒否する。逆に、検査の実行中は保守を開始できない（保守側が「使用中」で失敗する）。

- 既定（論理集計）は大きな DB でも数秒で終わる。`files`（DB/WAL/SHM のバイト数）、`tables`（表ごとの行数と文字列・BLOB 列の長さ合計）、`events`（kind/subkind 別の行数と payload バイト数。`subkind` 未補完の行は `(unbackfilled)`）、`run_contexts`（`messages` は header の平文バイト数と `run_messages.skeleton` の格納バイト数・圧縮済みメッセージ行数、`checkpoints` は header の値）、`conversation_index`（状態別の `charged_bytes`）、`threads`（active/archived 数）を返す。
- `attribution` は events・run_contexts・会話索引を `active` / `archived` / `orphan`（run がどのスレッドにも解決されない、または解決先の `threads` 行が無い）/ `unattributed`（`run_id` なし）に分ける。run の帰属は GUI のスレッド表示と同じ規則（`thread_runs` の束縛、`chat:<Role>:<thread>` の run 名、スレッドが記録する root run、保存済み context と usage ledger の親リンクをたどる）で解決し、各行はちょうど 1 つのスレッドに数える。`top_threads` は events・run_contexts・索引の合計バイト数が大きい上位 `--top` 件（既定 10）のスレッドを `thread_id` で示す。
- `--physical` は opt-in で遅い。`dbstat` による表・索引別のページサイズ（FTS の shadow 表は仮想表に集約）、`page_size`、`page_count`、`freelist_count`/`freelist_bytes`、`auto_vacuum`、`user_version` を加える。`freelist_bytes` と `unused_bytes` は縮小で回収できる余地の目安。

## 会話の保存形式（`run_messages`）

run の会話は、`run_contexts`（header: 構成・checkpoint 一覧・各種の射影列）と、provider の
メッセージ 1 件ごとの `run_messages` 行に保存する。2 KiB 以上の文字列値は内容アドレスの
`content_blobs`（zstd、縮む場合だけ採用）に置き、骨組みにはトークンだけが残る。保存は
変更のあったメッセージだけを書き、変わらないメッセージは metadata（`id`・`ordinal`・
`msg_sha256`）の読み取りだけで済む。読み出しは、トークンを本文に戻して同じ直列化で
連結するため、保存した JSON とバイト単位で一致する。

- **旧形式は開かない。** schema 44 未満の DB（schema 40 未満は会話が `messages_json` 列にある）と、旧列が
  残ったまま止まった変換の途中状態は、`Database::open`・読み取り専用接続・
  `evorch inspect`・保守コマンドのいずれも `ConversionRequired` で拒否し、ファイルを変更しない。
- **`config_json` は正規化して保存する。** JSON として同じ内容で、キーは辞書順になる。
  メッセージと checkpoint 一覧のバイト一致の契約は変わらない。
- **調査。** `evorch inspect run <run-id> --full` は本文を解決して返す。
  `--messages <from>..<to>` は保存順の番号（System メッセージを含む。provenance の番号とは別）で
  範囲だけを復元する。`evorch inspect check-bodies [--verify]` は、参照表と token の一致に加え、
  メッセージの連番・件数・長さ・hash と header の射影列を検査する。
- **待機理由。** `evorch inspect run` の `recorded_state` は、その run の最新の `AgentRunStarted` /
  `AgentRunStateChanged` イベントが記録した位相を `events.id` の降順で返す。位相が Waiting のとき
  `waiting` に型付きの理由（wait tool / delegate / blocking 質問 / ターン完了後 / その他の入力境界）が
  入り、`recorded_at_ns` はそのイベントの時刻（待機の開始）。Waiting の `reason` は常に null。
  理由が記録される前の Waiting は `waiting_unrecorded: true` になる。ログが記録した値であり、
  プロセスの現在状態の問い合わせではない。
- **削除。** アーカイブ削除の回収は `run_messages` を header より先に消す。1 回の transaction は
  最大 500 行・skeleton 4 MiB・参照 2,000 行で、超える 1 件は排他フェーズへ送り、その run の
  header も排他フェーズで子の後に消す。

## スレッドの期限削除（storage 基盤）

アーカイブ済みスレッドを期限後に削除するための storage 側の基盤（schema v35）。
通常 core のオンライン確定は後述 PR2b の有界 engine が担う。以下の staging/`retention_finalize` は既存の低レベル保守 API の説明で、core/legacy 起動時の新規確定には使わない。
「いつ確定するか」（activation 後の tick、writer authority、設定）は runtime/host が担い、
ここでは storage が提供する契約だけを説明する。

- **tombstone が唯一の不可逆境界。** `Database::retention_finalize` が、family
  （fork / rewind / escalation の木と、その thread・run・task）の
  `thread_tombstones` / `run_tombstones` / `task_tombstones`、`reclaim_jobs`、
  `threads` / `thread_runs` の削除を 1 transaction で commit する。commit 後の
  abort は存在しない。family は分割しない。閉包は thread と run の関係（系譜、束縛、
  `chat:<Role>:<thread>` 名を含む正規の run→thread 規則、委譲、usage の親、escalation）を
  すべてたどり、循環・`MAX_RUN_LINKS` 超過・tombstone 済みへの到達は `FamilyUnresolved`
  で確定しない。探索（`retention_explore`）は `retention_staging` に作業リストを保存して
  起動をまたいで進む。各ノードの関係は rowid で区切った辺クエリの列で、止まった位置
  （クエリ番号と最後の rowid）を作業項目に保存するため、1 ノードの途中でも予算で止まり、
  1 回の呼び出しは予算（run の正規 thread 解決の最大 65 リンクを除く）を超えて読まない。
  呼び出し全体が 1 transaction で、失敗すると直前の再開可能な状態が残る。確定直前の
  不動点確認は同じ辺を予算内で読み直す。family が探索後に増えていれば `Stale`（増分を
  staging へ積む）、予算を使い切れば `OverBudget`（何も変えない）。未変更の family が
  必要とする行数は探索が読んだ行数と同じなので、`retention_finalize_budget` がそれに
  余裕を足した予算を返す。増えた family は `FinalizeBudget::doubled` で倍にして再試行すれば
  有限回で完遂する。
  探索が `FamilyUnresolved` と判断した family は staging を捨てるが、メンバーと理由を `retention_unresolved` に残し
  （探索のたびに作り直す）、表示用の `retention_schedule` はその理由を「保留」として返す（「次回起動時に削除予定」にしない）。
- **判定入力は sidebar。** `RetentionInput` は呼び手が排他ロック取得後に読んだ
  sidebar を渡す。DB の `threads` の `archived` / `pinned` と revision 列は mirror で、
  検索除外と表示に使い、判定には使わない。revision が NULL（未初期化）の系列は
  sidebar から初期同期し、その際 archive 時刻を新しくしない。同じ revision で値が
  割れたら保護側（archived / pinned）を採る。Runtime writer はこれらの列を変えない。
  host（GUI）は archive / pin の編集ごとに revision を採番して送る（`ThreadUpsert` の revision は必須で、
  採番前の記録は世代 0・時刻なし）。高い revision が置換し、低いものは無視し、同じ revision で
  値が割れたら保護側を採る。未採番の経路（`archive_unnumbered` / `pin_unnumbered` 列）は
  v38 で廃止した。
- **予算の意味。** `ExploreBudget` / `FinalizeBudget` が制限するのは新しく読む辺の数だけで、1 回の呼び出しの総 I/O ではない。
  2 つの family を併合するときの staging 行の付け替え、family 破棄（unresolved・機能の無効化）での staging 削除、
  探索済み family のメンバー一覧の読み出しは、以前に溜めた staging の量に比例する。起動時の排他フェーズで
  だけ呼ぶこと（通常回収の固定サイズ transaction とは別枠）。
- **除外。** pinned、評価サンプル（`eval_samples`）、owner が保持する thread
  （`owner_blocks`）、family に非 due・未アーカイブ・sidebar 未知のメンバーがいる場合は
  確定しない。`archived_delete_after_days = 0` は新規確定を行わない（staging は破棄、
  mirror は同期する）。時刻のない legacy アーカイブは `Immediate`（即 due）。
  `event` の `run_id` / Orchestrator の帰属補完（backfill の第 2 対象）が終わるまで
  探索も確定も `Exploring` で止まる。
- **確定後の見え方。** 読み手（`thread`、`threads_in_project`、`thread_children`、
  `thread_of_run`、`runs_of_thread`、`run_context*`、`events_for_runs`、イベント走査、
  ledger、host input receipt、review evidence・lineage、assistant reference、user question、
  thread start・評価サンプル、索引の参照解決、`evorch inspect` の events・run、agent message、task、会話検索の候補、学習用の context 読み出し）は、回収の前でも tombstone 済みの行を返さない。
  返し続けるのは意図して残すもの（usage、学習記録、`storage` の物理集計、legacy の `messages` / `agent_runs`、予約済み ID を返す `persisted_run_ids`）だけ。
  owner と root の両方を持つ表（質問、host input receipt、review evidence、thread start）は、どちらかが削除済みなら返さない。
  削除済みの run の質問を生きている run へ bind することも拒否する。
  書き手は `StorageError::Tombstoned`（events、recorder は `skipped_tombstoned` に数えて
  再試行しない）または fence トリガの ABORT で拒否される。fence は INSERT と
  「生存へ戻す」UPDATE だけを止め、DELETE は常に許す。events の Orchestrator
  スナップショット（thread 単位）は SQL ではなく writer が Rust 側で拒否する。
- **回収。** `StorageHandle::reclaim_step` が `reclaim_jobs` の cursor を進め、1 transaction
  ごとに有界な量（events 500 行／4 MiB、小さな表 500 行、ledger と todo は走査 2,000 行、
  索引 200 行／2 MiB）を削除する。削除と cursor・カウンタの更新は同じ transaction で、
  中断しても再実行で残りだけを処理する。pending が無ければ何も書かない。
  通常 step の予算を超える単一の行（4 MiB を超える `run_contexts`、`thread_start_states.diff`
  や撮影済みファイル、event、2 MiB を超える索引 block）は暗黙に通さず `reclaim_oversized`
  に登録（大きさはその 1 行のもの。同じ run・capture の他の行は読まない）し、起動時の排他枠で
  `Database::reclaim_oversized_step` が有界な 1 batch（索引 200 行／2 MiB、captured file 500 行／4 MiB、
  または超過した 1 行だけ）ずつ削除する。残りがある登録は消えず、次の呼び出しが続ける。予算が小さくても
  最低 1 batch。`rows` / `stored_bytes` と job の `reclaimed_*` は実際に消した分だけを数える。
  失敗した行は登録が残る。索引の終端（run の保留中 request target を gap にする更新）も同じ batch の行数予算に含め、
  終わるまで run の索引行は `deleted` にならない。次の登録は `stored_bytes` 順の索引から選び、
  1 回の呼び出しが見る登録は 256 件までで、先頭の登録がすべて削除済みなら `rows == 0` のまま
  `remaining > 0` で返る。終了条件は常に `remaining == 0`。`reclaim_step` / `reclaim_oversized_step` が返す件数
  （`jobs_*`、`oversized_pending`、`remaining`）は 1,000 で頭打ちになり、job や登録の数に比例して読まない。実行可能な job と排他待ちの job は `waiting_exclusive` 列と専用の部分索引で
  別々に引く。gap にした request target は「更新した行」で、`rows` / `reclaimed_rows` には数えない。
  tasks・links は tombstone を task id 順の cursor でたどり、ledger と todo の走査は
  行ごとに tombstone を点引きするため、family の大きさで 1 step の読み取りは増えない。通常 step の残りが無く排他回収だけを待つ job
  （`jobs_waiting_exclusive`）は transaction を消費せず、後ろの job を止めない。`reclaim_mark_files` はファイル削除の結果を
  `db_done → files_pending → done | files_failed` に記録する。
- **残すもの。** usage（`usage_requests` / `usage_daily` / `usage_run_threads`）、学習系
  （claims、improvement、memory）、索引の要求履歴、`sessions` と集計、`eval_samples`。
  run に帰属できない events（Ownership、Diagnostic、routing など）も残る。全表の扱いは
  `repo/thread_retention/catalog.rs` に一覧があり、テストがスキーマと突き合わせる。
- **会計。** 回収は events の日次・session 集計、`sessions.total_event_bytes`、索引の
  `charged_bytes` を同じ transaction で更新する。tombstone 済み run の索引行は fence のため
  最後の block を消すまで課金を減らさず、最後に `deleted` として 0 にする。

## スレッドの期限削除（runtime：起動フェーズ・束縛・回収）

`crates/runtime/src/thread_retention/` が、上の storage 基盤を「いつ確定し、誰が回収するか」へつなぐ。
新規確定の駆動は core の background tick で、GUI 接続・再起動に依存しない。設定は
`[storage.thread_retention] archived_delete_after_days`（既定 7、0 で新規の確定を行わない。
時刻のない legacy アーカイブも、活動 cutoff と案 C・追加保護の検査を省略しない）。

- **保守ロックは 2 本。** DB のリース（`<db>.maintenance-lock`）と ownership root のリース
  （`<root>/.ownership-lock`）。稼働中のプロセスは両方を共有で、起動フェーズは両方を排他で持つ
  （取得順は DB → root。保守 lease の root が取れなければ DB も手放す）。core の専用 writer lease と root の任意取得は別の契約（PR2a/PR2b 節）。両方取れた起動だけが、他の evorch プロセスが
  存在しないと言える。`ExclusiveLeases::downgrade` は flock の変換が原子的でないため、共有へ戻した
  **後に** tombstone と束縛を読み直す。`Storage::open_with_lease` が DB のリースを writer スレッドへ渡す。
  回収の実行者は `<db>.reclaim-lock` を排他で持つ 1 プロセスだけ。
- **新規確定は active core のオンライン engine だけ。** authority・binding・設定・closure・案 C を再検査し、同一 IMMEDIATE writer transaction で seal→tombstone/job→commit→共通 guard 公開→seal 解放を行う（詳細は PR2b 節）。起動時の新規候補探索・owner 分類・予算倍増 fallback は行わない。
- **起動フェーズは回収専用。** `run_exclusive_phase` は束縛確認、bounded backfill、確定済み tombstone の owner/scratch 整理、巨大行、既存 incremental vacuum、artifact blob GC を行う。legacy embedded でも新規確定しない。回収は件数で区切り、crash・失敗で削除を取り消さず、次の起動で再開する。
- **保守は保持期間と束縛に依存しない。** backfill と確定済み owner/scratch・巨大行・vacuum の回収は `archived_delete_after_days = 0` でも動き、
  他の変更が登録した巨大行（`reclaim_oversized`）も回収する。巨大行回収と vacuum は DB だけを触るので、束縛が
  不一致の起動でも動く（`Skipped { BindingMismatch, maintenance }`）。owner 行と scratch は動かない。
- **scratch は実際に消えたことで完了とする。** `ThreadScratch::remove_thread(thread, max_entries)` は管理
  領域内を link をたどらずに 1 entry ずつ同期的に消し、`complete` は dir 自体を消せたときだけ true。
  予算（`scratch_dirs` と `scratch_entries`）に収まらない tree は部分的に残り、`ScratchReport.incomplete` に
  載って次の起動が続ける。権限などの失敗は `failed`（path 付き）で、`removed` に数えない。失敗するまでに消した
  entry も予算から引く（失敗しても後続の dir が予算を超えて消すことはない）。存在確認も同じ規則で、
  明示的な NotFound だけが「無い」を意味し、それ以外の失敗は `failed` として報告する。
- **archive 時の scratch 回収（稼働中）。** 削除確定とは別に、family を archive した時点で scratch を
  解放する。既存の archive 可否判定は変えず、新たな拒否や run の停止は追加しない。host の `SetFamilyArchived` と native GUI の archive は、sidebar の保存成功後・保存ロック解放後・
  添付削除の前に、family の全 thread の ID だけをヒントとして ownership host へ送る（archived=true の再要求でも
  送る。保存失敗・添付 Busy・restore では送らない）。ヒントはメモリに集約するだけで、sidebar の読み・
  ownership write transaction での検証・rename は UI スレッドにも host command にも置かず、専用の archive スレッドが行う。
  archive の成否と応答は回収に依存しない。使われていない thread は rename して `.trash-*` に退避し、物理削除は
  非同期の collector が行う。利用中の thread は全利用者が終わってから回収し、完了した turn の入力待ちは idle とみなす。
  tool/question 待ちは idle ではなく、run 本体・子 run・shell job を個別の identity で保護する。shell job は終端かつ
  teardown 確認済みになってから解除する。
  再開すると同じパスを再 prepare し、消失通知を一度だけ追記する。判定する checker は host と GUI が
  `workbench_host::scratch_archive::install` で注入し、共有 sidebar を保存ロックなしの `read_unlocked` で読む。
  family を解決できない thread、共有 sidebar がない構成、読取エラーは削除せず保持し、既知の unarchived はヒントを捨てる。
  添付の削除 API・ロック・root には触れない。**`.trash-*` がディスク上の正本**で、起動時と通知時に回収する。
  負荷は 256 entry ごとに 2 ms、Busy は 1 回だけ 100 ms 待って再試行する。失敗 tree は同じ pass で再試行せず、
  自己 wake も行わず、次の通知または起動で再試行する。通常 shutdown では producer の停止・
  drain・owner の release の後に `stop_scratch_cleanup` を呼ぶ。既存の restart/takeover 時の reset の契約は変わらない。
  既知の制限: 他プロセスが owner の archived thread はこのプロセスでは回収されず残り得る。保留中のヒントはメモリだけ
  なので planned handoff で失われ、次の release か通常起動で回収する。archive の成功と物理削除の完了は別で、
  後者は遅れる。削除できない tree は診断を出して残る。
- **進捗。** `run_exclusive_phase_with_progress` が回収段の開始・終了を呼び出し元のスレッドで
  順に通知する（`tracing` の `thread_retention` target にも info で出す）。
- **owner 行は削除許可に使わない。** 稼働中の案 C 判定は owners.db を読まない。起動時排他保守は、既に commit された tombstone に限り owner 行の残骸を整理する。
- **束縛。** DB と ownership root の組を最初の自動 root 整理より前に両側に記録する（DB: `retention_identity` と
  `retention_binding`、root: `.evorch-retention-binding.json`。root 側を先に durable に書き、DB 側を writer で確定して一致を確認する。上書きしない）。
  どちらかが欠ける、`db_id` が違う、sidebar（`--state` で別ファイルを渡した起動を含む）や root が違う場合は
  不一致で、新規の確定・owner 行の除去・scratch の削除をしない。DB 側の回収、artifacts の meta の削除、
  検索除外は続く。rebind は提供しない。root のファイルが同じ DB を指し DB 側の行がないだけの状態は、
  最初の試みが途中で止まった跡として再開する。
- **回収（`ReclaimExecutor`）。** reclaim-lock を持つプロセスが 5 秒ごとに backfill の残り、`reclaim_step`、
  `db_done` の family の artifacts の meta の削除（`ArtifactStore::remove_meta_for`）を行う。meta の走査は
  結果を区別する: 全件を調べ終えた（`stop == None`）、予算切れ（`Budget`。`last_name` から続ける）、一覧の取得失敗
  （`Listing`。完了ではない）、個別の読取・削除の失敗（`failed`。不在とは扱わない）。不在として許すのは
  明示的な `NotFound` だけ。files は、失敗ゼロで全件を調べ終えたときだけ `done` になり、失敗は
  `files_failed` として再試行する（20 回で打ち切って報告）。job は family key のカーソルで巡回し、
  retry 上限に達した family があっても後ろの family が止まらない。1 回の走査は一覧に全 entry を 1 回
  読むが、保持する名前と開くファイルは予算（既定 5,000/周回）以内で、1 ファイルは 1 MiB までしか読まない。
  artifacts の store を持たないプロセスは files に触れない。meta を消したら同じ artifact の参照行
  （`artifact_refs`）も消す（失敗しても files の判定は変えない。残った行は次の起動の孤立 ref 確認が消す）。
  blob は触らない（起動フェーズの段 7 だけが消す）。通常稼働中は owners.db と
  scratch を触らない。停止は `shutdown()` で join してから storage を閉じる。
- **ガード。** 起動時 tombstone とオンライン commit を同じ更新可能 guard に反映し、削除済みの thread・run を受け取る実行入口を
  `ThreadDeleted` で拒否する: `OwnerHost::start / claim / attach`、`bind_thread_root`、
  `create_thread_goal` / `restore_thread_goal` / `restore_thread_todo`、
  `set_thread_read_only_directories`、run の復元と継続（`resume_chat` / `wake_chat` / `continue_goal`）、
  `delegate_chat(_seeded)`、fork seed、escalation seed、host input の配送、保存済み root の compaction、
  質問への回答。公開入口と内部 wake・cached admission・再活性化の一覧と、それぞれの拒否方法（または不要な理由）は
  `thread_retention/tests/entries.rs` の表で固定し、新しい入口が増えるとテストが落ちる。
- **`--state` で別の sidebar を使う起動は束縛不一致になり、自動確定が無効になる。**

## artifact の実体ファイル回収（起動フェーズの段 7）

アーカイブ削除で消えるのは `artifacts/meta/artifact-<id>.json` だけで、実体の
`artifacts/blobs/<sha256>.<ext>` は内容アドレスのため複数 thread の meta が共有しうる。段 7 は、どの meta も
指さなくなった blob を mark & sweep で消す。参照の正本は DB の `artifact_refs`（1 artifact 1 行）。

- **capture の順序。** blob を書く → 参照行を DB に記録して commit を待つ → meta を公開する。記録に失敗したら
  meta を書かず `Err` を返す（blob だけが残り、次の sweep が回収する）。meta が見えるなら必ず行がある。
  この順序は参照を記録する store（`runtime::artifacts::store_with_refs(root, handle)`）で有効になり、GUI はこれを使う。
  記録しない store（テスト・headless）の meta は、次の起動の mark が補う。
- **mark。** meta の名前を順に見て、参照行も `gc_unknown_meta` の記録も無い meta を開いて行にする（1 起動で
  `gc_meta_files`（既定 20,000）件・`gc_meta_bytes`（32 MiB）まで。読み取り途中で失敗した分の bytes も数える）。
  既知の meta は DB の索引引きだけで開かない。1 起動で名前を見る数は `gc_meta_lookups`（100,000）まで（保持する名前も
  この数まで）。それを超える `meta/` は、位置（`gc_state.meta_cursor`）と「その位置までの名前の指紋」
  （`meta_count` と、各名前の SHA-256 を 2^256 で法として足した値の 16 進 `meta_hash`）を保存して**複数回の起動に分けて**走査する。起動のたびに `meta/` 全体を列挙し直し、
  位置までの名前の指紋が保存値と違えば（参照を記録しない host が位置より前に meta を足した可能性があるので）
  pass を最初からやり直す。位置より後ろの追加はこれから見るので影響しない。完了した pass の次の起動は新しい pass で、
  記録なしで追加された meta も sweep の前に必ず見つかる。やり直しの原因は位置より前の追加・削除なので、
  指紋は変更の検出器であり（名前を細工しない限り見逃しは変更あたり約 2^-256）、`meta/` に任意の名前を書ける
  相手に対する同一性の証明ではない（その相手は任意の meta も書ける）。
  `lookups` を超える巨大な store で pass の途中に meta の増減が毎回起きると完了しない（予算内の store では起きない）。
- **参照が完全（`refs_complete`）になる条件。** pass が最後の名前を越え（予算で止まらず、位置までの指紋も一致し）、
  参照のない meta を全部読めて行にでき、`gc_unknown_meta` に残る meta が 0 件のとき**だけ**。
  列挙の失敗・開けない・読み取り途中の失敗・1 MiB 超・JSON として壊れている・`artifact_id` が名前と違う・
  `sha256` が小文字 64 桁の 16 進でない・ディレクトリや dangling link である meta は、どの blob を守って
  いるか分からないので `gc_unknown_meta` に記録し、**修復するか消すまで全 blob の sweep を止める**。
  `meta/` 自体が無い／読めない場合も同じ。DB エラーも「参照なし」とは読まず、段を打ち切る
  （`GcStop::Database`）。明示的な NotFound（かつ、その entry 自体が無い）だけが「無い」を意味する。
- **孤立した参照行。** meta の無い行を `gc_ref_checks`（20,000）件ずつ `ref_cursor` から確認して消す。
  `meta/` を列挙できたときだけ行い、判断できない（NotFound 以外の失敗）行は残す。
- **sweep。** `refs_complete` のときだけ。`blobs/` を名前順に `blob_cursor` 以降 `gc_blob_entries`
  （50,000）件まで調べ、その sha256 を持つ行がなければ（`gc_unlinks` = 5,000 件まで）unlink する。
  対象は `<小文字 64 桁 hex>.<英数字の拡張子>` の通常ファイルだけ（`.partial-*.tmp`、ディレクトリ、symlink、
  他の名前には触らない）。共有 blob は、最後の参照が消えるまで残る。起動フェーズ（両 EX 保持）の中だけで動くので、
  capture と競合しない。
- **再開。** どの小段も件数で区切り、途中で止まっても（crash を含む）次の起動が同じ手順をやり直せる
  （unlink は冪等、NotFound は成功扱い）。報告は `StartupOutcome::Ran.gc`（`GcReport`: 追加した行数・
  残る unknown 件数と先頭数件のエラー・unlink 数と解放 bytes・`failed`・`stops`）。DB の削除とは別の完了項目で、
  `refs_complete = false` のあいだ blob は残るだけで他は何も壊れない。
- **運用。** meta が `gc_unknown_meta` に残り続ける間は blob が回収されない。報告の `unknown_sample` のファイルを
  修復するか削除すれば、次の起動で解消する。

## スレッドの期限削除（GUI：起動・表示・採番）

通常 GUI は core に接続し、新規削除確定は core が行う。以下は legacy embedded の起動回収と共通の表示・sidebar 規則。legacy の起動回収は新規確定をしない。

- **起動の 3 段。** ① `prepare()`（引数・設定・DB と ownership root と sidebar のパスを確定。ロックは持たない）
  → ウィンドウを開く。② 背景スレッド（`gui::startup`）が DB・root の排他ロックを取り（`try_exclusive`）、
  sidebar を読み、起動フェーズを走らせ、共有ロックへ `downgrade` して tombstone と束縛を読み直す。
  進捗はスプラッシュ（`gui::app::splash`）に段ごとに出る（● 実行中 / ✓ 完了、確定した family 数、経過時間）。
  フェーズが終わるまでウィンドウの閉じる操作は無視される。他のプロセスがロックを持っていれば（`NoExclusive`）
  フェーズは飛ばして通常起動し、`--demo` はフェーズを行わない。③ 1 フレーム内で `build_workbench`
  （`Storage::open_with_lease` → 参照を記録する artifact store（`store_with_refs`）→ `OwnerHost::open` → tombstone guard → 回収 executor → 履歴の復元）。
  段 7（artifact の実体回収）もスプラッシュに段として出る。
  ②・③の失敗はスプラッシュと stderr に出て、ウィンドウを閉じるとコード 1 で終わる。
- **sidebar の採番（family が状態を持つ）。** archive と pin は **family 単位**で、スレッド記録は状態を持たない。
  family = `parent_thread_id` をたどった最上位の祖先から降りる全員（fork・rewind の旧版・escalation の子と、その子孫）。
  キー = 最上位の祖先の ID で、表示中の版や選択中のスレッドに依存しない。sidebar.json の `families`
  （`FamilyState { key, archived, archived_at, archive_revision, pinned, pin_revision }`）が **archive 系列 1 本・pin 系列 1 本**を持つ
  （状態の無い family は未 archive・未 pin・revision 0）。スレッド記録の `archived` / `archived_at` / `pinned` は、
  `SidebarState::refresh_derived`（唯一の書き手）が family から導いて表示に使う**派生値で、保存しない**。
  GUI・DB への mirror・起動時の保守入力・60 秒ごとの schedule は、すべて `SidebarState::resolve_states`（同じ解決関数）を読む。
  - **採番する操作は archive / unarchive / pin / unpin だけ。** 保存ロックの中で最新の sidebar（ファイル＋手元の
    3-way merge）に対して family の該当系列だけを採番して書き（`shared_sidebar_transact`）、直後に DB へ mirror する。
    archive は pin を、pin は archive を動かさない。pin は family 全体（過去の版・fork・escalation を含む）に効く。
    ロックを取れなければ手元の状態は変えず、エラーを表示する。no-op の操作でも、他者が保存した内容は取り込む。
  - **子の追加と版切替は所属を足すだけ。** fork・rewind・escalation の子の作成と版切替は、状態をコピーも採番もしない
    （子は family の状態をそのまま読む）。fork・rewind・版切替は、ID の重複を防ぐため保存ロックの中で行い、ロックを取れない・
    ファイルを読めない場合は何も作らない。escalation の子はイベントの畳み込み中に作るので失敗させず、手元に足して
    次の保存で merge する（状態を持たないので、ロックが取れない間も状態は食い違わない）。
  - **merge。** `families` はキーの和集合で、系列ごとに revision の高い側を採る（同値で内容が割れたら保護側 =
    archived / pinned。archive の時刻が 2 つあれば遅い方。どちらが local でも同じ結果）。どちらか一方にしか無い状態は残す。古い画面の再追加・replay・import・再起動で、保存済みの revision は戻らない。
  - **DB への射影。** DB のスレッド単位の列は、family 状態からの射影（派生）。family の状態が変わると、全メンバーの
    upsert を作り直して送る（新しいメンバーにも同じ状態が付く）。再送は冪等で、プロセスが落ちても次の起動が sidebar から再構成する。
    **射影の元は「確定した snapshot」だけ**: このプロセスが最後に保存・import に成功した結果（起動時は読み込んだファイル）で、
    未保存の子や取り込み前の merge 結果は含めない（ロックが取れない間の escalation の子は、保存に成功するまで DB に出ない。
    保存の merge で他者の新しい family 状態が入っていれば、その状態で最初の行が作られ、検索対象にならない）。
    共有ファイルを持たない構成（テスト）だけは手元の状態から射影する。削除の保留表示（schedule）も同じ snapshot を使う。
  - **解決できない所属。** 親が sidebar に無い、親の鎖が輪になっている、同じ ID に異なる親を与えた 2 つの sidebar を merge した（`parent_conflicts` に残し、子孫も含めて保留）、のスレッドは「解決不能」。状態を持たず
    （未 archive・未 pin として表示）、archive / pin の操作は拒否し、DB へは archive / pin を送らず（DB の既存値を保つ）、
    削除の入力では未 archive として送るので、その family の削除は保留になる。archive 一覧のホバーは、
    保留の理由を "the family of related thread X cannot be resolved" と出す。
  - **一度きりの変換（sidebar version 1 → 2）。** 起動の排他フェーズで、旧形式（スレッドごとの archived / pinned）を
    読んだときに行い、保存してから判定に使う。archive は family 全員が archived のときだけ archived
    （`archived_at` = 持っているメンバーの最大。時刻を持つメンバーが無ければ時刻なしのまま = 即時対象）。
    **混在（一部だけ archived）の family は未 archive にし、新しい時刻は作らない。** pin は 1 人でも pinned なら pinned。
    revision は旧メンバーの最大値と DB 側の値（スレッド行の revision）の最大より 1 大きくして始め、DB に無視されない。
    変換した family 数・混在数・解決不能数はスプラッシュに出る。以後は変換しない。
    **変換・DB の revision との突き合わせ・保存は排他取得側だけ**が行う。排他を取れない起動（他のプロセスが DB を保持）で
    旧形式の sidebar を見つけたときは、変換した状態を公開も保存もせず起動を失敗させる（他のウィンドウを閉じて再起動する）。
    新形式（version 2）の `NoExclusive` 起動は従来どおり。起動後に旧形式のファイルを読んでも（古いプログラムの書込み）変換せず、
    family の状態を持たないファイルとして merge する。
  - sidebar.json を読めない（壊れている）ときは、archive / pin の操作も通常の保存も、ファイルを上書きせずエラーにする
    （他のウィンドウの pin / archive を確認できないまま古い状態を書くと、後続の削除判定に使われるため）。
    import が失敗したときも、通常保存は同じファイルを読み直して merge し、読めない間は上書きしない。
- **確定後の片付け。** 再読込した tombstone に載るスレッドは、sidebar・`user_messages`・下書き・選択から外れ、
  sidebar の 3-way merge・import・復元・保存直前（merge しない保存も）でも戻らない（tombstone が merge の
  「他方の編集を残す」規則に優先する）。
- **guard。** core では runtime・OwnerHost・host・planned 経路が同一の更新可能 guard を共有する。legacy embedded は起動時の同じ tombstone snapshot を runtime と OwnerHost に設定する（新規確定しない）。
  runtime への設定は、目標・手順の復元など runtime の入口を使う前に行い、復元関数は設定した証拠を引数に取る。
- **表示。** アーカイブ一覧のタイトルのホバーに "Deletes in N days" / "Waiting for online cleanup" /
  "Deletion on hold (reason)" を出す（owner が保持している間は "owner active until <ローカル日時>"）（機能が無効なら何も出さない。「今すぐ削除」はない）。状態は背景スレッドが
  起動後・60 秒ごと・archive / unarchive / pin の後に計算し、探索済みの family の保留理由（pin・owner・
  未期限の関連スレッド・系統の未解決・探索中）も含む。束縛が不一致のときは "Deletion disabled (configuration mismatch)"。
  GUI の文言は既存の GUI に合わせて英語。
- **通知。** 起動後に確定があった（または失敗があった）ときだけ "Startup maintenance: deleted N threads
  (M runs); about X MB will be reclaimed. Skipped: …" を 1 件出す（artifact の実体を消したときは
  "removed N unused artifact files (X MB)."、読めない artifact 記録で回収が止まっているときは Skipped に件数）。束縛不一致は "Thread deletion is disabled
  (configuration mismatch: reason)"。`NoExclusive` は通知せず、ログだけ。

## 既知の制約

- 縮小は停止中の DB が前提。`compact-events` は preview でも稼働中の store を拒否し、`backfill-events` だけが稼働中に使える。リース（`.maintenance-lock`）を迂回する手段はない。
- バックアップ無効化オプションはなく、空き容量は DB サイズの 4 倍（`required_free_space_bytes`）が目安。圧縮の見積もりは最大 1,000 値の標本推定で、最終サイズは保証されない。
- 既存の単独 64 KiB 超行は分割せず `oversized_rows` に報告する。sidebar を読めない場合は delta の集約が無効になる（他の工程は続行する）。
- wall_clock が非単調で、削除されるイベントだけがユーザー入力の挿入時刻判定を越える履歴は、表示順の保持対象外。
- `--delta-mode delete` は reasoning 表示と未完了の本文を失う。バックアップからしか戻せない。
- 長時間続く reader は、バックフィル中に WAL の増大を招く。
- 常時稼働中の live retention（[io-performance.md](io-performance.md#イベントの永続化と保持)）は 1 tick あたりの削除量と時間を制限するため、既存の大きな滞留は本手順で一括処理する。

## archive を妨げるものの整理（`cleanup-archive-blockers`）

自動 archive は未回答の質問と、閉じられていない managed lane（attached／summary）があるスレッドを保留する。
溜まった既存分を**一度だけ**片付けるオフライン専用コマンド。新しいオンライン API・writer コマンドは追加しない。

- 質問は新しい終端状態 **cancelled** になる。回答・回答元・承認・review evidence は作られず、回答済みでもない（`user_answers` / `subagent_questions` には `cancelled: true` で出る）。同じ transaction で、その質問の pending な lane Question 通知を obsolete にする（lead を起こさない）。
- lane は既存の close 遷移で **discarded** になる（closer は `maintenance_cli`）。run の停止・workspace・報告状態は変えない。後日 lane が再 open されたら通常どおり open 扱いに戻り、古い plan では再度 close されない。
- detached lane と既に有効な close がある lane は対象外。
- 取消・close は他の保護（pin・goal・最近の活動など）を解除しない。保護が外れたスレッドは自動 archive され、既定ではその **7 日後に自動削除**され得る。

### 手順

1. evorch（GUI・core・CLI）を完全に停止する。GUI を閉じただけで止まったと判断しない。
2. 新しい evorch で一度起動して正常終了し、schema を最新にし WAL を空にする（旧 schema・非空 WAL は preview・apply とも拒否される。このコマンドは migration も WAL 回復もしない。WAL・lock ファイルを手で消さない）。
3. preview（読み取り専用。DB・WAL・SHM・lease・config・lock・`sidebar.json` を作成も変更もしない）の JSON を保存して読む。

```sh
CONFIG="$HOME/.config/evorch"
umask 077
evorch maintenance cleanup-archive-blockers --config-dir "$CONFIG" > /safe/place/cleanup-preview.json
```

   確認する項目: 対象の質問・lane の全件、除外と理由、処理不能（`unprocessable`）、`apply_allowed`、新しい Closed 通知の宛先 lead 数、3 つの hash（`source_image_sha256`・`target_manifest_sha256`・`plan_hash`）。`config-dir` 以外の DB を指す `--db` には対応する `--config-dir`（その core lock と `sidebar.json`）が必須。
4. **preview は apply の許可ではない。** 内容を確認し、了承してから同じ plan と新しい backup 先で apply する。

```sh
evorch maintenance cleanup-archive-blockers --config-dir "$CONFIG" --apply \
  --plan /safe/place/cleanup-preview.json --backup /safe/place/evorch-before-cleanup.db
```

`--backup` に指定したパスにまだファイルが無ければ、検証済みの新しい backup を作る。あれば、同じ plan の image と byte 一致する場合だけ再利用し、上書きしない。backup は元データを含むので保管に注意する（DB 本体のみで、sidebar・artifact は含まない）。

### 排他・安全の契約

- apply は config の launch／lifetime lock、DB の保守 lease、SQLite の排他接続を終了まで保持する。core が動作中・起動中なら拒否される。preview は lock ファイルが無ければ「core 未使用」として DB の lease に委ね、他の異常は拒否する。
- 未適用の apply は「WAL が空」「main DB の SHA-256 が plan と一致」「再列挙した対象が plan の manifest と一致」のすべてが必要。**preview と apply の間に core を一度でも起動したら、再 preview が必要。**
- 質問が requester・root・全継承先の run がすべて削除済みで、かつ DB（`thread_runs`）にも sidebar の `run_ids`／`root_run_id` にも生きた thread の束縛が無い場合だけ「残骸」として除外される。判定できない項目（`sidebar.json` が読めない・形式不明、削除済み thread に宛てた pending 通知など）は処理不能で、1 件でもあれば apply 全体を拒否する。
- 全変更は backup の検証後に単一 transaction で行い、完了 receipt（hash と件数だけ）を保存する。終了時に `wal_checkpoint(TRUNCATE)` を行う。

### 再実行・中断

- commit 前の中断は全体が rollback される。同じ plan で再実行できる（検証済み backup は再利用される）。
- 完了済みの plan を再実行すると `already_applied` を返し、何も変更しない。
- commit 後に checkpoint が終わらなかった場合は、通常の起動 → 正常終了 → **同じ plan で apply を再実行**して `already_applied` を確認する。
- 上記以外（receipt なしで DB が変わっている等）は無変更で拒否される。再 preview から始める。

### writer lease（PR2a）

apply は config の launch／lifetime lock と DB 保守 lease に加え、core の **DB writer 専用 lease（`<db>.writer-lock`）と root writer 専用 lease（`<ownership root>/.ownership-writer-lock`）を DB → root の順で排他取得**してから WAL を確認する。待機はせず、core・legacy embedded writer・同じ ownership root を共有する別 DB の core のいずれかが動いていれば無変更で拒否する。ownership root は core と同じ `$XDG_STATE_HOME/evorch/threads`（未設定なら `~/.local/state/evorch/threads`）。ディレクトリが無ければ作成して root lease を実際に取得し、apply の終了まで保持する（作成も取得もできなければ拒否する）。preview は lease を取らず、lock ファイルも作らない。

## thread 自動 archive・オンライン削除（PR2a/PR2b）

```toml
[storage.thread_retention]
auto_archive_after_days = 7
archived_delete_after_days = 7
```

- それぞれ独立に 0 で新規の archive／削除確定を無効化する。明示した 30 などの値は維持する。確定済み削除の回収や確定済み archive の添付整理は、必要な authority があれば続く。
- **削除の省略時既定値は 7 日。7〜30 日前の既存 archive も、生存中 core のオンライン保守で削除対象になり得る。** 30 日を維持したい場合は明示する。core の readiness 前には新規候補の一括探索・確定をしない。時刻のない旧 archive も、直近活動と保護を省略せず判定する。
- **通常の単一 core では、自動 archive・添付整理・scratch hint がクライアント接続なしでも動く。** 専用 DB writer lease（`.writer-lock`）は排他必須。競合時は async executor 外で最大 100ms の待機予算（10ms × 10 回、短命 writer の解放待ち）を使い、解放されなければ理由付きで起動を拒否する。maintenance lease の取得も残り予算を使い、core の lease 待機全体を同じ 100ms 予算に収める（既存の起動時排他処理自体の所要時間は別）。通常の前景 handler はこの待機に入らない。OS スケジューリングによる超過はあり得る。
- root writer lease（`.ownership-writer-lock`）は DB の後に一度だけ試行する。別 config-dir／別 DB の core が同じ ownership root を使う場合、後発も起動するが、自動 archive・オンライン削除確定・自動 root 配下整理を `AuthorityUnavailable` として無効にする。root に触れない既存 DB reclaim は継続する。先発終了後も昇格せず、再起動まで無効のまま。保留 tick は DB・sidebar・添付を開かず、host mutex も sidebar／添付 lock も取らない。起動ログに理由を出す。
- writer lease と既存 maintenance lease は別物。core と read-only inspect は maintenance を共有し、offline 保守は排他のまま。lock ファイルは unlink しない。直接 `Storage::open` と旧 `--legacy-embedded` は DB writer lease を共有取得するため、core 稼働中は接続案内付きで拒否される。新 lease を守らない古いバイナリとの同時利用は保証しない。
- 自動添付整理／scratch hint は、両 writer lease の authority と DB/root/sidebar の binding 一致が条件。Unbound は host lock の外で root 側を durable に書き、writer ACK で DB 側を確定し、再確認してから整理する。Mismatch でも archive 自体は保存するが、自動 root 配下操作はしない。symlink は辿らず root 外を消さない。
- planned successor も DB 必須／root 試行で取得し、adopt・activation 前に retention producer を起動しない。停止時は新規 tick 停止→worker join→storage barrier／終了の順で、writer・OwnerHost・producer の寿命を lease が覆う。保守は core の idle busy 理由を増やさず、core 終了後には処理しない。
- archive は従来の sidebar family、削除は既存 closure（親子・run binding・usage・lane management・未退役 recovery manifest 等）の全件かゼロ。後述のオンライン削除は archive と同じ案 C を再利用する。
- **判定は派生状態を持たない。** 1 tick は 1 family だけを、host mutex の `try_lock` と sidebar lock の同一区間で、正本（events・受理表・`run_contexts`・質問・評価サンプル・lane・`lane_handoffs`）から直接判定する。tick を跨いで持つのは巡回位置だけで、判定の途中結果・投影・backfill・trigger はない。v75 は索引 4 本だけを足す（表・trigger なし）。
- 判定の手順は、sidebar 再読込、admission の gate stamp（全体・thread・run）採取、メモリ上の確認（実行中 run、メモリ上の goal）、stamp 採取後に始めた 1 つの read transaction での DB 判定、stamp の一致確認、保存・mirror・通知の順。stamp 採取から再確認までの間に前景の admission や event の publish が入ると publish しない。再確認後の admission は archive 済み family 上の通常操作として扱う。
- 活動は、events の `run_id` ヘッダと受理表（`host_input_receipts`・`host_chat_submissions`）の `accepted_at_ns` で見る。種別は MessageCompleted／FinalResultPublished／MessagePart、TurnCompleted、Delivered（再送も数える）、Running への出入りを示す AgentRunStateChanged（この行だけ復号）。閲覧、sidebar 操作、goal/lane/usage、履歴再構築、handoff コピー（元の時刻を保つ）、復元時の再公開、質問取消、lane close は活動ではない。family の期限は全 member の最終活動の最大から 24 時間×日数（境界時刻以上で到達、overflow で早期処理しない）。
- 空 thread（run もメッセージもない）は作成時刻から日数で対象になる。event・受理行が 1 件もなく会話（`run_contexts.message_count > 0`）だけがある run は、その保存時刻を遅延方向の上界として使う。会話もない run は遅延させない。
- pin、実行・開始待ち・復元/wake/compaction 進行中、未完了 goal（Complete 以外）、評価サンプル、未回答質問（子・継承先含む）、open attached/summary lane、membership 投影の未適用は family 全体を保護する。通常のターン間待機は実行中と混同しない。`backfill_pending`（events ヘッダの backfill 未完）の間は全 family を保留する。
- goal は run ごとの最新 snapshot を部分索引 `idx_events_goal_by_run` で 1 件ずつ読み、thread の最新を現在の goal とする。`lane_handoffs` に committed 以降で同じ goal_id の receipt（元 run 起点）があれば、goal は移譲済みで移譲元を保護しない（移譲先での完了・置換には依存しない）。未 commit の receipt では保護する。
- 1 tick の予算は、family の thread＋run が R=1,024、run あたり末尾 P=16 行、tick あたり末尾 Q=512 行、goal 復号 K=128 件、読込み byte 2,097,152。events を探索する各文は `INDEXED BY` で索引を固定する（例外は、選んだ行の本文・shape を主キー（id／rowid）で 1 件読む文だけ）。超過・復号失敗・family 外の thread を名指す snapshot は保留（`FamilyRunBudget`／`ActivityProbeBudget`／`GoalDecodeBudget`／`GoalUndecodable`／`GoalOutsideFamily`）。
- 自動処理は専用スレッドで host mutex を try_lock できたときだけ動く。前景の handler、Busy 契約、手動 archive の attachment→sidebar lock と同期削除は維持する。最終再検査から rename までの残る admission 窓は許容する。新しい intent 保留や arbitration は置かない。
- 1 tick は最大 1 family の判定、1 publish、`remove_thread` 1 呼出し（既存 4096 entry 有界削除）、scratch hint 1 member。sidebar の archived 状態が添付整理の唯一の durable intent で、cursor はメモリ上。restore は従来どおり残存添付の整理を先に完了する。symlink を辿らず root 外を消さない。失敗で archive 時刻を戻さない。
- scratch は手動と同じ既存 hint のみ。hint 再送は許容するが、プロセス内状態・lease の制約により古い thread などを回収できない場合がある。hint 発行は scratch 削除の durable 完了を意味しない。
- 期限処理は idle の busy 理由ではない。保守だけのために core を常駐させない（`idle_grace_secs` 既定 60 秒）。停止中は別 daemon が処理せず、次回稼働時に再開する。planned exit の準備時は既存 ProducerGate で auto-archive owner を pause／drain し、停止時は新規 tick を止めて storage barrier の前に join する。ReclaimWorker は変更しない。
- 診断は `thread_retention` target の構造化ログと有界 `RetentionReport`。本文は記録せず、同じ理由を毎 tick 再出力しない。`AttachmentCleanupPending` は未完了と失敗をまとめた区分。

### 既知の限界

遅延方向（手動 archive は可能）:

- receipt のない移譲（v54 より前、テスト用の非 durable 経路）の移譲元は goal 保護のまま。
- R を超える family は自動 archive されない。
- 直近の日数内に活動でない event が P 行を超える run を持つ family は遅れる。
- fork で複製された会話（event なし）は、復元時の保存で遅れる。
- 1 つの移譲元 run に goal の `lane_handoffs` receipt が K 件を超えて積まれている場合は保留（`GoalDecodeBudget`）。

誤 archive 方向（許容済みの範囲）:

- 時計が archive 日数以上巻き戻った場合。
- read transaction 開始後に gate を通らず commit された変更（authority 下では他プロセスの writer はいない）。
- `run_id` ヘッダと食い違う payload だけの活動帰属変更（header を正本とする）。
- 同一 run 内で header の `wall_clock_ns` を ID 順と逆転させる帯域外 rewrite（古い ID の時刻だけを新しくするなど）。判定は run の最大 ID から末尾を読んで打ち切るため、直近の活動を見落とす。本番の events 書込み経路（追記、delta 圧縮、payload の rewrite、backfill）は `wall_clock_ns` を書き換えず、delta 圧縮も先頭行の id と時刻を保つため、この逆転を作らない。

### 計測

判定 SQL 一式は `cargo run --release -p storage --example retention_decision_measure -- <work-dir>` で測る（合成 fixture のみ、実データは使わない）。基準・予算は計測前に宣言し、結果は PR に記録する。Sidebar の lock 区間は、1,000 threads が p95 50ms 以下・全観測最大 100ms 以下、2,000 threads は全観測最大 100ms 以下（常識外のブロックがないことの確認）。PR2 の性能は同じ観点で記録・確認する。

### オンライン削除の保護と既知の限界（PR2b）

- core 生存中、クライアントが 0 でも archive と削除の巡回を交互に進める。削除期限は各 member の archive 時刻＋設定日数の最も遅い時刻で、同じ日数内の活動がある closure は保留する。期限表示は最早期限の概算で、削除の許可ではない。
- writer の IMMEDIATE transaction で closure 固定点・全 member の archive/期限/pin・案 C・追加保護・binding を再確認する。案 C の未完了 goal・未回答質問・open lane は、手動 archive の削除も保護する。評価サンプル、未適用 membership 投影、restart/recovery evidence、prepared claim、未退役 manifest、未解決 lineage/backfill も保護する。オンライン確定は owners.db の旧 owner/claim 分類を使わない。
- 期限と保護をすべて通過した closure のターン間待機 run だけ、既存の停止経路で terminal 化する。進行中ターン・未処理入力・未解決 admission・compaction があれば停止も確定もしない。入力と停止は同じ受理 serializer で原子的に順序付ける。停止 event の永続化と実 task 終了を確認してから、改めて確定する。停止が新しい活動や親への完了 wake を生まない。停止後に pin 等で確定できなくなっても自動再開せず、次のユーザー入力による既存の復元経路に任せる。task 終了の証拠を持たない terminal entry も保留する。
- 最終 stamp 比較と seal は同じ memory mutex 内で行う。queue 待ちは seal しない。同期 writer request と bus publish は 1 commit の seal 解放まで待ち、Busy による内部失敗・イベント破棄にしない。commit 成功後、共有の更新可能 tombstone guard に公開してから解放する。rollback/error/panic は解放し、commit 失敗は guard に公開しない。削除後の terminal entry は実行できず、sidebar に復活させない。sidebar 保存/通知の後処理失敗は tombstone を正本に次 tick で修復する。
- 1 候補の予算は member（thread＋run）64、edge 2,048、task 256、副作用行 1,024、1 MiB。上限超過 closure は部分削除せず、理由付きの恒久保留・診断にする。起動時の予算倍増 fallback はない。保留・失敗・巨大行待ちが後続候補を止めないよう巡回する。v77 はオンライン SQL の索引のみで、新しい永続判定表・trigger・cursor は作らない。
- 既存 reclaim job/cursor を使い、オンライン writer request は 1 transaction ずつ、共通の送信枠で積み上げず処理する。添付は authority＋matching binding の下で、sidebar から消えていても tombstone から再開する。通常行・参照・添付・freelist の回収は段階的で、ファイルサイズの即時縮小を保証しない。vacuum は既存 maintenance()/run_budgeted_vacuum のみ。全 VACUUM や reader を待つ強制 WAL TRUNCATE は追加しない。
- 巨大単一行、artifact 全域 GC、旧 owner 行と scratch ディレクトリの全域整理は起動時排他保守に残す。オンライン経路は owner 行や scratch ディレクトリを直接消さない。巨大 closure の救済のための起動時削除確定はしない。
- 保守は idle busy 理由を増やさず、idle_grace_secs=60 で core が終了した後には処理しない。短い稼働だけでは backlog が残り、次回 activation で再開する。停止は新規要求停止→進行中要求の writer 応答→producer join→storage barrier の順。常駐 daemon は追加しない。

新規削除確定は core のみ。旧 embedded 起動 wrapper は確定済み tombstone/job の回収専用で、owner 判定・候補探索・予算倍増を行わない。
