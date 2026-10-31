# ストレージ保守手順

イベント DB（`evorch-events.db`）を縮小・整備する `evorch maintenance` の
`compact-events` と `backfill-events` の手順書。コマンド一覧は
`evorch maintenance` の USAGE（`crates/evorch/src/maintenance.rs`）が正本。
永続化されるイベントの種別と保持方針は
[io-performance.md](io-performance.md#イベントの永続化と保持) を、タスク進捗の
保守は [task-progress-maintenance.md](task-progress-maintenance.md) を参照。

## 使い分け

- `evorch maintenance backfill-events` は、既存イベントの `subkind` / `run_id` 列だけを埋める軽量コマンド。GUI/runtime の稼働中にも使用できる。DB の物理サイズ縮小や delta の集約は行わない。
- `evorch maintenance compact-events` は、既定で読み取り専用 preview。実際の変換には `--apply` が必要。preview と apply の両方で、対象 DB を使うすべての GUI/runtime/reader を停止する。
- どちらも runtime やモデル呼び出しを開始せず、暗黙の schema migration は行わない。compact は schema 31 以上、backfill は schema 30 以上が必要。古い DB は、まず停止中のバックアップを別途取得し、現行 evorch の通常起動による migration 後に正常終了してから保守する。

以下の `$DB` は対象を確認した DB の絶対パスに置き換える。既定パスはユーザー設定ディレクトリ内の `evorch-events.db`。

## オンラインの列バックフィル

```sh
evorch maintenance backfill-events --db "$DB" --batch-rows 1000 --max-rows 50000
```

JSON の `converted` / `undecodable` / `batches` / `remaining` を確認する。`remaining` が 0 になるまで繰り返せる。`--max-rows` を省くと全未処理行を対象とし、0 を指定すると変換せず残数だけを返す。`--batch-rows` は 1～100,000、既定 5,000。

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
5. 成功 JSON の `integrity_check: "ok"`、`vacuum: true`、`checkpoint_truncated: true`、`after.unbackfilled_rows: 0` を確認する。種別別件数・ファイルサイズ・`elapsed_seconds` を記録し、GUI の履歴表示を確認してからバックアップの保存方針を決める。

## 変換内容と保持契約

apply の順序は固定：

1. `Database::backfill_event_columns` で未バックフィル列を埋める。復号不能行は `subkind=''` として残す。
2. SubscriberLagged と RunProgress を削除する。
3. MessageDelta を run 別・応答境界別に集約する。
4. ReasoningDelta を同じ帰属・境界規則で集約する。
5. Provider の RequestCompleted / FirstTokenObserved / CacheReuseObserved の期限切れ行だけを削除する。
6. 既存 codec と writer と共通の判定で Tool / Message / Compaction payload と run_contexts の平文列を圧縮する。2 KiB 未満、圧縮で縮まない値、AgentMessage、承認等の payload 帰属 Tool は平文のまま。
7. セッション・日次の event bytes と `sessions.total_event_bytes` を再計算する。
8. VACUUM、WAL TRUNCATE、integrity_check を行う。

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

## 既知の制約

- 縮小は停止中の DB が前提。`compact-events` は preview でも稼働中の store を拒否し、`backfill-events` だけが稼働中に使える。リース（`.maintenance-lock`）を迂回する手段はない。
- バックアップ無効化オプションはなく、空き容量は DB サイズの 4 倍（`required_free_space_bytes`）が目安。圧縮の見積もりは最大 1,000 値の標本推定で、最終サイズは保証されない。
- 既存の単独 64 KiB 超行は分割せず `oversized_rows` に報告する。sidebar を読めない場合は delta の集約が無効になる（他の工程は続行する）。
- wall_clock が非単調で、削除されるイベントだけがユーザー入力の挿入時刻判定を越える履歴は、表示順の保持対象外。
- `--delta-mode delete` は reasoning 表示と未完了の本文を失う。バックアップからしか戻せない。
- 長時間続く reader は、バックフィル中に WAL の増大を招く。
- 常時稼働中の live retention（[io-performance.md](io-performance.md#イベントの永続化と保持)）は 1 tick あたりの削除量と時間を制限するため、既存の大きな滞留は本手順で一括処理する。
