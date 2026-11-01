# ストレージ保守手順

イベント DB（`evorch-events.db`）を縮小・整備する `evorch maintenance` の
`compact-events`、`backfill-events`、`vacuum` の手順書。コマンド一覧は
`evorch maintenance` の USAGE（`crates/evorch/src/maintenance.rs`）が正本。
永続化されるイベントの種別と保持方針は
[io-performance.md](io-performance.md#イベントの永続化と保持) を、タスク進捗の
保守は [task-progress-maintenance.md](task-progress-maintenance.md) を参照。

## 使い分け

- `evorch maintenance backfill-events` は、既存イベントの `subkind` / `run_id` 列だけを埋める軽量コマンド。GUI/runtime の稼働中にも使用できる。DB の物理サイズ縮小や delta の集約は行わない。
- `evorch maintenance compact-events` は、既定で読み取り専用 preview。実際の変換には `--apply` が必要。preview と apply の両方で、対象 DB を使うすべての GUI/runtime/reader を停止する。
- `evorch maintenance vacuum` は、旧 DB の `auto_vacuum` を INCREMENTAL に一度だけ切り替える軽量コマンド（[auto_vacuum の一回限りの移行](#auto_vacuum-の一回限りの移行)）。イベントの変換は行わない。
- いずれも runtime やモデル呼び出しを開始せず、暗黙の schema migration は行わない。compact は schema 31 以上、backfill は schema 30 以上が必要。古い DB は、まず停止中のバックアップを別途取得し、現行 evorch の通常起動による migration 後に正常終了してから保守する。

以下の `$DB` は対象を確認した DB の絶対パスに置き換える。既定パスはユーザー設定ディレクトリ内の `evorch-events.db`。

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

   `before` / `after` は実在する snapshot なので、preview では同一のまま。変換後の種別別行数は `projected_event_rows` を参照する。各工程の `input_rows` / `output_rows` / payload bytes、圧縮サンプル、`backfill.remaining`、`oversized_rows`、`required_free_space_bytes` を確認する。events の圧縮サンプルは、削除・集約を仮想的に適用した後に生存する平文 payload が対象（小さい delta の集約で 2 KiB を超える値も含む）。events と run_contexts それぞれ最大 1,000 値からの推定であり、DB の最終物理サイズを保証しない。
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

- 既定（論理集計）は大きな DB でも数秒で終わる。`files`（DB/WAL/SHM のバイト数）、`tables`（表ごとの行数と文字列・BLOB 列の長さ合計）、`events`（kind/subkind 別の行数と payload バイト数。`subkind` 未補完の行は `(unbackfilled)`）、`run_contexts`（`messages`/`checkpoints` の平文バイト数と格納バイト数、圧縮済み行数）、`conversation_index`（状態別の `charged_bytes`）、`threads`（active/archived 数）を返す。
- `attribution` は events・run_contexts・会話索引を `active` / `archived` / `orphan`（run がどのスレッドにも解決されない、または解決先の `threads` 行が無い）/ `unattributed`（`run_id` なし）に分ける。run の帰属は GUI のスレッド表示と同じ規則（`thread_runs` の束縛、`chat:<Role>:<thread>` の run 名、スレッドが記録する root run、保存済み context と usage ledger の親リンクをたどる）で解決し、各行はちょうど 1 つのスレッドに数える。`top_threads` は events・run_contexts・索引の合計バイト数が大きい上位 `--top` 件（既定 10）のスレッドを `thread_id` で示す。
- `--physical` は opt-in で遅い。`dbstat` による表・索引別のページサイズ（FTS の shadow 表は仮想表に集約）、`page_size`、`page_count`、`freelist_count`/`freelist_bytes`、`auto_vacuum`、`user_version` を加える。`freelist_bytes` と `unused_bytes` は縮小で回収できる余地の目安。

## スレッドの期限削除（storage 基盤）

アーカイブ済みスレッドを期限後に削除するための storage 側の基盤（schema v35）。
「いつ確定するか」（起動時の排他フェーズ、ロック、設定）は次の節の runtime が担い、
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
GUI は起動順に呼ぶだけで、判定と実行は runtime / storage にある。設定は
`[storage.thread_retention] archived_delete_after_days`（既定 30、0 で新規の確定を行わない。
時刻のない legacy アーカイブは日数に関係なく即 due で、pin・評価サンプル・実行中の family は除外）。

- **ロックは 2 本。** DB のリース（`<db>.maintenance-lock`）と ownership root のリース
  （`<root>/.ownership-lock`）。稼働中のプロセスは両方を共有で、起動フェーズは両方を排他で持つ
  （取得順は DB → root。root が取れなければ DB も手放す）。両方取れた起動だけが、他の evorch プロセスが
  存在しないと言える。`ExclusiveLeases::downgrade` は flock の変換が原子的でないため、共有へ戻した
  **後に** tombstone と束縛を読み直す。`Storage::open_with_lease` が DB のリースを writer スレッドへ渡す。
  回収の実行者は `<db>.reclaim-lock` を排他で持つ 1 プロセスだけ。
- **確定は起動フェーズ（`run_exclusive_phase`）だけ。** 段 0 束縛確認 → 1 backfill → 2 探索 →
  3 owner 分類・root 束縛・family ごとの確定 → 4 owners.db と scratch の削除 → 5 巨大行 → 6 incremental vacuum → 7 artifact の blob 回収
  （下の節。artifacts ディレクトリがあり、束縛が不一致でないときだけ）。
  量（行数・辺数・件数）で区切り、実時間では区切らない。残りは次の起動が続ける。段 0〜3 は確定の commit まで
  DB の外を変えない。段 4 以降は確定済みの集合に対する回収で、失敗や crash でも削除は取り消されず、
  次の起動が同じ段をやり直す。family は分割しない（`OverBudget` は予算を倍にして最大 4 回、足りなければ次回）。
  確定後に DB エラーが起きても `Err` にせず、確定済みの family と失敗を `StartupOutcome::Ran` に載せて返す。
- **保守は保持期間と束縛に依存しない。** 段 1・4・5・6 は `archived_delete_after_days = 0` でも動き、
  他の変更が登録した巨大行（`reclaim_oversized`）も回収する。段 5・6 は DB だけを触るので、束縛が
  不一致の起動でも動く（`Skipped { BindingMismatch, maintenance }`）。owner 行と scratch は動かない。
- **scratch は実際に消えたことで完了とする。** `ThreadScratch::remove_thread(thread, max_entries)` は管理
  領域内を link をたどらずに 1 entry ずつ同期的に消し、`complete` は dir 自体を消せたときだけ true。
  予算（`scratch_dirs` と `scratch_entries`）に収まらない tree は部分的に残り、`ScratchReport.incomplete` に
  載って次の起動が続ける。権限などの失敗は `failed`（path 付き）で、`removed` に数えない。失敗するまでに消した
  entry も予算から引く（失敗しても後続の dir が予算を超えて消すことはない）。存在確認も同じ規則で、
  明示的な NotFound だけが「無い」を意味し、それ以外の失敗は `failed` として報告する。
- **進捗。** `run_exclusive_phase_with_progress` が段の開始・終了、root 束縛、family の確定を呼び出し元のスレッドで
  順に通知する（`tracing` の `thread_retention` target にも info で出す）。
- **owner の残骸。** 両ロックの下に生きたプロセスはないので、owners.db の行はすべて残骸として扱う。
  released / idle は確定でき、turn や run を持つ行は、lease の期限切れから保持期間（日数）が経つまで
  `Blocked(Owner)`。owners.db が「無い」のは明示的な NotFound のときだけで、調べられない（symlink loop など）・
  読めない場合は何も確定しない。パスの解決（`canonical_string`）も NotFound の最終要素だけを許し、他の失敗は
  エラーにする。
- **束縛。** DB と ownership root の組を最初の確定で両側に記録する（DB: `retention_identity` と
  `retention_binding`、root: `.evorch-retention-binding.json`。root 側は最初の確定の直前に書き、上書きしない）。
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
- **ガード。** 起動後に tombstone を 1 回読み、削除済みの thread・run を外から受け取る入口を
  `ThreadDeleted` で拒否する: `OwnerHost::start / claim / attach`、`bind_thread_root`、
  `create_thread_goal` / `restore_thread_goal` / `restore_thread_todo`、
  `set_thread_read_only_directories`、run の復元と継続（`resume_chat` / `wake_chat` / `continue_goal`）、
  `delegate_chat(_seeded)`、fork seed、escalation seed、host input の配送、保存済み root の compaction、
  質問への回答。公開入口の一覧と、それぞれの拒否方法（または不要な理由）は
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

`evorch-gui` は起動を 3 段に分け、削除機能を有効にする。判定と削除は runtime / storage にあり、GUI は
駆動・表示・sidebar の片付けだけを担う。

- **起動の 3 段。** ① `prepare()`（引数・設定・DB と ownership root と sidebar のパスを確定。ロックは持たない）
  → ウィンドウを開く。② 背景スレッド（`gui::startup`）が DB・root の排他ロックを取り（`try_exclusive`）、
  sidebar を読み、起動フェーズを走らせ、共有ロックへ `downgrade` して tombstone と束縛を読み直す。
  進捗はスプラッシュ（`gui::app::splash`）に段ごとに出る（● 実行中 / ✓ 完了、確定した family 数、経過時間）。
  フェーズが終わるまでウィンドウの閉じる操作は無視される。他のプロセスがロックを持っていれば（`NoExclusive`）
  フェーズは飛ばして通常起動し、`--demo` はフェーズを行わない。③ 1 フレーム内で `build_workbench`
  （`Storage::open_with_lease` → 参照を記録する artifact store（`store_with_refs`）→ `OwnerHost::open` → tombstone guard → 回収 executor → 履歴の復元）。
  段 7（artifact の実体回収）もスプラッシュに段として出る。
  ②・③の失敗はスプラッシュと stderr に出て、ウィンドウを閉じるとコード 1 で終わる。
- **sidebar の採番。** `ThreadRecord` に `archived_at`（unix 秒）・`archive_revision`・`pin_revision` を持つ。
  archive / unarchive / pin は保存ロックの中で最新の sidebar（ファイル＋手元の 3-way merge）に対して
  適用して採番し（`shared_sidebar_transact`）、書き込み、直後に DB へ mirror する。古い画面での操作が
  他のウィンドウの archive を取り消すことはない。ロックを取れなければ手元の状態は変えず、エラーを表示する。
  merge は archive 系列・pin 系列をそれぞれ revision の高い側を丸ごと採る（同値で割れたら保護側）。
  fork・rewind・版切替も同じ transact の中で行い（新しい記録の作成と、親または切替前の版の archive / pin 状態の継承を
  最新の sidebar に対して確定する）、ロックを取れない・ファイルを読めない場合は何も作らず失敗する。sidebar.json を
  読めない（壊れている）ときは、archive / pin の操作も通常の保存も、ファイルを上書きせずエラーにする（他のウィンドウの
  pin / archive を確認できないまま古い状態を書くと、次回起動の削除判定に使われるため）。escalation の子はイベントの
  畳み込み中に作るため失敗させられない: transact（書込みのみで手元へは取り込まない）で親の最新の状態を継承し、
  ロックを取れなければ中立の状態（未 archive・未 pin・revision 0）で `state_pending` を立てて作る。このフラグは
  sidebar.json に記録の一部として保存されるため、どのウィンドウも（再起動後も）ロックを取れた時点でまとめて
  1 つの transact で親の状態を継承して解除する（1 秒間隔で再試行）。フラグの立った記録は、どの GUI からも DB へ
  mirror しない。親も pending の子は親が確定するまで待つ。保留判定の入力ではこの子は未 archive として読まれ、
  family は保留になる。sidebar.json が import できなかったときも、通常保存は同じファイルを読み直して merge し、
  読めない間は上書きしない。
- **確定後の片付け。** 再読込した tombstone に載るスレッドは、sidebar・`user_messages`・下書き・選択から外れ、
  sidebar の 3-way merge・import・復元・保存直前（merge しない保存も）でも戻らない（tombstone が merge の
  「他方の編集を残す」規則に優先する）。
- **guard。** 再読込した tombstone は `AgentRuntime` と `OwnerHost` の両方に設定する（別々の保存先）。
  runtime への設定は、目標・手順の復元など runtime の入口を使う前に行い、復元関数は設定した証拠を引数に取る。
- **表示。** アーカイブ一覧のタイトルのホバーに "Deletes in N days" / "Deletes at next startup" /
  "Deletion on hold (reason)" を出す（owner が保持している間は "owner active until <ローカル日時>"）（機能が無効なら何も出さない。「今すぐ削除」はない）。状態は背景スレッドが
  起動後・60 秒ごと・archive / unarchive / pin の後に計算し、探索済みの family の保留理由（pin・owner・
  未期限の関連スレッド・系統の未解決・探索中）も含む。束縛が不一致のときは "Deletion disabled (configuration mismatch)"。
  GUI の文言は既存の GUI に合わせて英語。
- **通知。** 起動後に確定があった（または失敗があった）ときだけ "Startup maintenance: deleted N threads
  (M runs); about X MB will be reclaimed. Skipped: …" を 1 件出す（artifact の実体を消したときは
  "removed N unused artifact files (X MB)."、読めない artifact 記録で回収が止まっているときは Skipped に件数）。束縛不一致は "Thread deletion is disabled
  (configuration mismatch: reason)"。`NoExclusive` は通知せず、ログだけ。

実 DB のコピーでの確認は `cargo run -p gui --example startup_phase_check -- --db COPY/evorch-events.db --root COPY/threads --sidebar COPY/sidebar.json [--days 30] [--reclaim-cycles N]`
（窓なしで起動フェーズ・sidebar の片付け・背景回収を走らせる。**原本には使わない**）。

## 既知の制約

- 縮小は停止中の DB が前提。`compact-events` は preview でも稼働中の store を拒否し、`backfill-events` だけが稼働中に使える。リース（`.maintenance-lock`）を迂回する手段はない。
- バックアップ無効化オプションはなく、空き容量は DB サイズの 4 倍（`required_free_space_bytes`）が目安。圧縮の見積もりは最大 1,000 値の標本推定で、最終サイズは保証されない。
- 既存の単独 64 KiB 超行は分割せず `oversized_rows` に報告する。sidebar を読めない場合は delta の集約が無効になる（他の工程は続行する）。
- wall_clock が非単調で、削除されるイベントだけがユーザー入力の挿入時刻判定を越える履歴は、表示順の保持対象外。
- `--delta-mode delete` は reasoning 表示と未完了の本文を失う。バックアップからしか戻せない。
- 長時間続く reader は、バックフィル中に WAL の増大を招く。
- 常時稼働中の live retention（[io-performance.md](io-performance.md#イベントの永続化と保持)）は 1 tick あたりの削除量と時間を制限するため、既存の大きな滞留は本手順で一括処理する。
