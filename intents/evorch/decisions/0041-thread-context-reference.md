# ADR 0041: 他スレッドの会話を参照する（thread_search / thread_grep / thread_read）

## Status

Accepted（2026-10-24）。利用者との相談で、以下の設計判断（a〜i と権限モデル、Phase A の設計）をすべて推奨案どおりに承認した。Phase A の A-1〜A-3 は実装済みで、任意の A-4 は今回は見送る。進捗は末尾の「実装状況」に記録する。

この ADR は、evorch の再起動で作業が失われても、次の担当がこの ADR だけを読んで再開できるように、承認済みの仕様をすべて書く。

- 着手順は Phase 0（ADR と計測）→ Phase A（storage / runtime / GUI）である。Phase A は PR #351 として squash マージ済み（merge commit `5d23cab4`、2026-10-25）。Phase B（本文の全文索引）は、Phase 0 の計測と利用者との相談を経て着手が決まり、設計を下の「Phase B の設計」に記録した。Phase C（ポインタ注入・GUI 添付）は今回の範囲外である。
- Phase 0 の計測結果は、以下の「Phase 0 計測」節に記録する。計測は完了している。Phase B の実装の進捗は末尾の「Phase B 実装状況チェックリスト」に記録する。

## Context

エージェントが別スレッドの会話を参照できると、過去の決定や委譲の経緯を再利用できる。現状、他スレッドの内容を読む経路はない。

- `history_read` は `self` と `handoff:<run-id>` だけを扱う。run ID や GUI のスレッド ID を渡すと `history_source_denied` で拒否され、次の手はユーザーに内容を貼ってもらうことだけになる（#330 で案内文を追加済み）。
- `run_output` は直接の子と親の出力しか読めない。
- 会話の実体は SQLite の store（`run_contexts`、`usage_run_threads`、`review_evidence`、`user_questions` など）にあるが、スレッドという単位の表はない。スレッドの定義（タイトル、プロジェクト、アーカイブなど）は GUI の `sidebar.json` だけにある。runtime はスレッド単位の情報を持たず、run の名前規約（`chat:<Role>:<thread>`）と親子関係から間接的にしか辿れない。

一方で、スレッド ID や run ID を推測しただけで別プロジェクトや他人の会話を読める設計は許容できない。ID の形式は推測しやすく、会話には secret やプライベートな内容が含まれうる。また、SQLite の生 SQL をモデルに渡すと、認可・secret の除去・出力量の制御をすべて迂回してしまう。

そこで、既存の会話データを正規化して、読み取り専用で権限つきの 3 つのツールとして公開する。

## Decision

### 概要

全ロール共通の読み取り専用ツール `thread_search`、`thread_grep`、`thread_read` を追加する（決定 h）。

- 返り値の参照形式は `run-…#<ordinal>.<block>`。
- すべての出力に、会話内容は信頼できない入力（untrusted）である旨の注記を付ける。
- 出力上限は 16 KiB。
- 生 SQL はモデルに出さない（決定 g）。
- 他スレッドの会話の自動注入（ポインタ含む）は今回しない（決定 f、Phase C）。

### 権限モデル（visibility）

あるスレッド（または run）が、呼び出した run から見えるのは次の集合 V(run) に限る。

1. 自スレッド。
2. 同じ canonical `project_root` のスレッド。
3. 系譜: fork / rewind の元、escalation の子と元。
4. ユーザーが ID を与えたもの（決定 a）。

スレッドを持たない root（CLI 起動など）は、プロジェクト範囲（2）だけを見られる。

拒否文は、対象が存在しない場合と存在するが見えない場合で同一にする。存在の有無を漏らさないためである。文言は次の 1 つに固定する。

```
thread_denied: <id> is not visible from this run; a thread outside your project becomes readable only when the user names its ID in this conversation
```

#### 決定 a: プロジェクト外のスレッドを読める条件

対象 ID（`thread_id` または `run_id`）が、自スレッド系譜の**人間由来の証拠**に**完全一致**で含まれるときだけ、プロジェクト外のスレッドを読める。

- 人間由来の証拠は、`review_evidence` の `initial_prompt` / `host_chat` / `user_answer`、および `trusted_thread_request` である。
- `working_note`、`tool_result`、他のエージェントの発言は証拠に数えない。エージェントが書いた文字列を経由して許可が広がるのを防ぐためである。
- ID の形式が正しいだけでは許可しない。部分一致・前方一致も許可しない。
- GUI の添付による指定は Phase C で扱う。

#### 決定 b: escalation 先

escalation 先は元スレッドの子として扱い、元と先を双方向に読める。対象は thread ツールだけである。`run_output` と `wait` の挙動は変えない。

### スレッド定義とデータモデル

#### 決定 c: スレッド定義の正と同期

- スレッド定義の正は `sidebar.json` とする。GUI から DB の `threads` へ一方向にミラーする。同期間隔は `usage_run_threads` と同じ 1 秒ごとである（前例: `crates/gui/src/app/usage_ledger.rs` の `sync_usage_ledger`）。
- runtime も、自分が起動・分岐したスレッドについて `thread_id`、`root_run_id`、`project_root`、escalation の系譜を `threads` に書く。
- host（GUI）と runtime が同じスレッドを書くため、列ごとに `COALESCE` して、host が書いた値を runtime が上書きしない。`source` 列は `runtime` / `host` / `backfill` で由来を示す。
- プロジェクトの主キーは canonical な `project_root` とする。`ProjectId` は表示と補完のための値であり、権限判定の鍵にしない。

### 検索

#### 決定 d: 検索方式

- 本文検索（Phase B）は FTS5 の trigram を使う。2 文字以下の語は trigram で引けないため、`LIKE` で代用する（Phase B の設計では「3 scalar 未満の語」と定義する）。
- Phase A では FTS を使わない。Phase B の詳細は「Phase B の設計」を参照。
- `thread_grep` はスレッド内の正規化したテキストを線形に走査する。
- `thread_search` はメタデータ（title、人間の発話、working note、最終報告）だけを対象にする。

#### 決定 e: 子 run と tool_result

- 子 run は既定で検索・走査の対象に含める。
- `tool_result` は順位を下げ、`collapse_repeats` で同じ内容の反復を畳む。

### 出力の安全性（secret と除外）

- 出力は必ず `SecretRedactor`（`crates/secret-guard`）を通す。
- thinking / reasoning のブロックは出力から除外する。
- 画像は内容を出さず注記に置き換える。
- すべての出力に untrusted の注記を付ける。

### history_read との関係

`history_read` は `self` と `handoff:<run-id>` だけを扱う（変更しない）。他スレッドの参照は `thread_read` など thread ツールで行う。両者の権限モデルは独立で、混ぜない。

`crates/runtime/src/meta/history.rs` の `denied()` の案内に「他スレッドは `thread_read` を使う」という 1 文を追加する。現在の案内の「他スレッド・他 run の履歴は読めない。ユーザーに貼ってもらう」という趣旨の文は、この追加と矛盾しないように言い回しを直す。`history_source_denied` の先頭コードと既存の `did you mean` は保つ。

### Backfill

既存のデータ（`run_contexts` の名前規約と親子関係、`EscalationRequested`、`usage_run_threads`、`thread_start_states` など）から `threads` / `thread_runs` を補完する。補完した行は `source='backfill'` とする。

- 同期で行うか非同期で行うかの判断は、Phase 0 の計測結果と実装結果を踏まえ、以下のとおり同期実行に決定した。
- **backfill は v24 マイグレーション内で同期実行する。** 根拠: 実 store コピー（2.1 GB、1,638 run）への v24 適用は 2.4〜7.3 秒（一度だけ。時間の大半は escalation 補完の events 全走査）で、Python 参考値による 1 run 正規化 p95 は 8.5 ms。初回起動の一度限りのコストとして許容する。
- `project_root` が null の旧 run（実測 687 件）の扱いは、補完できるものと補完できないものに分ける。補完は root / 親、`usage_run_threads`、`thread_start_states`、config の cwd から推定できる場合に限る。推定できない run は `project_root` を null のままにし、同じスレッドの範囲か、ユーザーが ID を与えた場合（決定 a）にだけ読めるものとする。プロジェクト範囲の可視性（2）では見えない。計測では確定的な補完根拠を持つものは 0 件だった。`usage_run_threads` に該当 run は 652 件あるが、持つのは `project_id` のみで canonical `project_root` の根拠にならない。

### 段階計画（決定 i）

| 段階 | 内容 |
|---|---|
| Phase 0 | ADR と計測 |
| Phase A | storage / runtime / GUI。FTS なし。thread_search はメタデータのみ |
| Phase B | 本文の全文索引（FTS5 trigram）。設計は「Phase B の設計」、進捗は「Phase B 実装状況チェックリスト」 |
| Phase C | ポインタ注入、GUI 添付による ID 指定。今回の範囲外 |

## Phase 0 計測

実 store `/home/turtton/.config/evorch/evorch-events.db` を `mode=ro` で開いて計測した。本番 DB は書き換えていない。計測結果は次のとおり。

| 対象 store | `/home/turtton/.config/evorch/evorch-events.db`（SQLite `mode=ro`） |
| 計測日時 | 2026-10-24 07:14:00 UTC |
| DB サイズ | 2,138,054,656 bytes（約 1.99 GiB、計測時の DB ファイル） |
| run 総数 | 1,638（`run_contexts`） |
| 除外後の本文量 | 294,648,246 bytes（UTF-8）、1,638 runs、135,511 messages |
| 本文 kind 分布 | user: 6,311 件 / 10,062,617 bytes; assistant text: 6,695 / 8,556,912; tool_use: 72,076 / 39,031,050; tool_result: 72,076 / 229,295,607; system: 3,705 / 7,702,060; その他（assistant の空/除外ブロック等）: 0 bytes |
| 10% サンプルでの FTS5 trigram 索引サイズ | 1,638 run 中 164 run（無作為、seed=410）。抽出本文 25,604,480 bytes、SQLite DB ファイル 87,478,272 bytes（本文量の 3.4165×）。単純 10 倍外挿: 874,782,720 bytes（約 834 MiB、本文量の 3.4165×） |
| 1 run の正規化時間 | median 0.000722 s; p95 0.008461 s; max 0.058946 s。大きい run: `run-01m5hq577mqemk417aetfsh9f1` 1,494,253 bytes / 666 messages; `run-01m4r36vbk0rt33t9a8yzdw1hh` 1,294,982 / 554; `run-302` 1,293,618 / 352 |
| `project_root` が null の旧 run と補完 | null: 687 run。親/祖先、`thread_start_states.project_root`、`usage_run_threads` 経由の root、config の cwd による確定的な補完はいずれも 0。補完可能合計 0、未解決 687。`usage_run_threads` がある null run は 652（`project_id` のみで canonical root の根拠にはならない）。 |

### 手法・注記

実 store は Python `sqlite3`（SQLite 3.53.3）の読み取り専用接続で調査した。本文は `messages_json` から thinking / reasoning / 画像 / 非表示・opaque block を除いて正規化した。FTS5 trigram は seed=410 で無作為抽出した 164 run の一時 SQLite DB ファイルのサイズ（DB 管理 overhead を含む）を測定し、全量値はその単純 10 倍である。正規化時間は Python の JSON parse・除外・テキスト化を `perf_counter` で測った参考値。補完は確定的な根拠のみを採用した。

### Phase 0 計測の要約（Phase B 向け）

出所は上の表と手法・注記のみである。計測用スクリプトや別の計測記録はリポジトリ内に見つからなかった（`Phase 0` / `phase0` / `phase_0` を grep したところ、この ADR だけが該当した）。そのため、以下はこの ADR の数値から導いた要約であり、新しい計測ではない。

- 除外後の本文量は 294,648,246 bytes（1,638 run、135,511 messages）で、DB 全体（約 1.99 GiB）の約 14% にあたる。
- 本文の大半は tool 由来である。tool_result が 229,295,607 bytes（約 77.8%）、tool_use が 39,031,050 bytes（約 13.2%）で、合わせて約 91.0%。user（10,062,617 bytes）と assistant text（8,556,912 bytes）は合計 18,619,529 bytes（約 6.3%）、system は 7,702,060 bytes（約 2.6%）である。
- そのため、tool_use と tool_result を切り詰める（Phase B の抽出契約）だけで索引対象の本文は大きく減る。切り詰め後の kind 別バイト数と索引サイズは、Phase B の計測で改めて測る（現時点の数値は未取得）。
- FTS5 trigram の索引サイズは、10% サンプル（164 run）で本文量の 3.4165 倍だった。全量（切り詰めなし）に単純外挿すると約 834 MiB になる。サンプルの DB ファイル実測値であり、管理 overhead を含む。
- 1 run の正規化は Python 参考値で median 0.72 ms、p95 8.5 ms、最大 59 ms。最大の run は本文 約 1.49 MB / 666 messages。Rust 実装の性能ではない。
- `project_root` が null の旧 run は 687 件で、確定的に補完できたものは 0 件。Phase B の可視性は Phase A の `threads` / `thread_runs` に従い、索引側で可視性を再定義しない。
- 上の 3.4165 倍は、切り詰めなしの本文に対する値である。切り詰め後の容量と、`max_bytes` の候補（256 MiB / 512 MiB / 1 GiB）ごとの保持範囲は Phase B の計測項目として残す（決定 B-2）。

## Phase A の設計

### A-1 storage

次のマイグレーション番号は `crates/storage/src/migrations.rs` を確認して決める。2026-10-24 時点の最新は v23 なので **v24** である（`crates/storage/src/migrations/v24.sql` を作り、`MIGRATIONS` に `include_str!` を足す）。

#### テーブル

`threads`

| 列 | 備考 |
|---|---|
| `thread_id` | PRIMARY KEY |
| `project_id` | 表示・補完用 |
| `project_root` | canonical。権限判定の鍵 |
| `title` | |
| `root_run_id` | |
| `chat_role` | |
| `parent_thread_id` | |
| `lineage_kind` | CHECK IN ('fork', 'rewind', 'escalation') |
| `fork_point_run_id` | |
| `fork_point_context_len` | |
| `escalation_source_run_id` | |
| `archived` | |
| `superseded` | |
| `created_at_ns` | |
| `updated_at_ns` | |
| `source` | CHECK IN ('runtime', 'host', 'backfill') |

インデックス: `(project_root, updated_at_ns DESC)` と `(parent_thread_id)`。

`thread_runs`: `run_id` PRIMARY KEY、`thread_id`、`root_run_id`、`depth`。

#### repo/threads.rs

`crates/storage/src/repo/threads.rs` に次を実装する。

- `upsert_thread_runtime`: runtime 側の書き込み。列ごとに `COALESCE` する。
- `upsert_threads_host`: host（GUI）側の書き込み。
- `thread`: 1 件取得。
- `threads_in_project`: プロジェクト内の一覧（cursor つき）。
- `thread_children`: 子スレッド。
- `thread_of_run`: run からスレッドを解決する。`thread_runs` → 名前規約 `chat:<Role>:<thread>` → parent 連鎖の順で引く（旧データの救済を兼ねる）。
- `runs_of_thread`
- `run_contexts_for`
- `latest_ledgers`
- `terminal_summaries`

#### writer

writer に `Command::UpsertThreads` と、ブロックしない `try_upsert_threads` を追加する（GUI の 1 秒同期が writer を待たせないため）。`RunContext` の保存時に `config_json.thread_id` があれば `thread_runs` を更新する。

#### 受け入れ基準

- 古い DB を migrate でき、`open_read_only` で開ける。
- upsert が冪等で、`COALESCE` が効く（host の値を runtime が上書きしない）。
- 旧データ（`threads` / `thread_runs` のない run）を `thread_of_run` で解決できる。

### A-2 runtime

#### 系譜の記録

- `RunRestoreDescriptor`（`crates/runtime/src/restore.rs`）に `thread_id: Option<String>` を追加する（`serde` の default つきで、古い記録を読める）。
- `thread_start.rs` と `spawn_escalated_root`（`crates/runtime/src/runtime.rs`）で、スレッドと系譜（fork / rewind / escalation）を書く。

#### meta/threads/

`crates/runtime/src/meta/threads/` に次を置く。

- `visibility`: 権限モデルの判定。拒否文は固定の `thread_denied` 文言。
- `normalize`: 会話を検索・表示用のテキストにする。
  - ordinal は `system` 以外のメッセージの序数とする。
  - `tool_use` と `tool_result` は `call_id` で組にする。
  - thinking は除外し、画像は注記に置き換える。
  - `SecretRedactor` を必須とする。
  - `collapse_repeats` を実装する。
- 3 つのツール（下記）。

#### thread_search

スレッドを単位に探す。

| 引数 | 内容 |
|---|---|
| `query` | 文字列、または OR 配列。省略すると更新の新しい順の一覧 |
| `match` | `words` / `phrase` / `regex` |
| `include_children` | 既定 true |
| `kinds` | 対象の kind の絞り込み |
| `updated_after` | |
| `limit` | 20 以下 |
| `cursor` | |

返り値は、スレッドごとに title / project / status / lineage / hit_count / matched_runs / snippets（各 200 字以下）/ next_cursor を持つ。

Phase A の検索対象はメタデータ（title、人間の発話、working note、最終報告）だけである。このことを tool description に明記する（本文の検索は `thread_grep` を使う）。

#### thread_grep

| 引数 | 内容 |
|---|---|
| `thread` | `thread_id` または `run_id` |
| `pattern` | |
| `regex` | |
| `runs` | 対象 run の絞り込み |
| `kinds` | |
| `context_chars` | 400 以下 |
| `max_hits` | 50 以下 |
| `collapse_repeats` | 既定 true |
| `cursor` | |

スレッド内の正規化テキストを線形に走査する。

#### thread_read

| 引数 | 内容 |
|---|---|
| `thread` | |
| `view` | `overview` / `messages`。既定は `overview` |
| `run` | |
| `around` | ref（`run-…#<ordinal>.<block>`） |
| `before` / `after` | 各 8 以下 |
| `max_bytes` | 既定 6 KiB、上限 16 KiB |
| `cursor` | |

`overview` に含めるもの:

- タイトル、プロジェクト、系譜。
- root run の phase、終了理由、最終報告の抜粋（600 B 以下）。
- 最新の working note。
- `user_questions`。
- 子 run の要約（name / role / phase / 最後のツールエラーの分類 / 終了理由）。
- 最後の 2 往復。
- 不整合の注記（store 上の矛盾など）。

`messages` は、値を二重エスケープしない入れ子の構造で返す。ref の形式は `run-…#<ordinal>.<block>` とし、`around` で同じ ref を指定すると安定して同じ位置を返す。

#### 登録

- `meta/registry.rs` の `define_meta_ops!` に追加する。
- `meta/specs.rs` の `object_spec` に、`HistoryRead` の直後に固定の順序で並べる。引数は `deny_unknown_fields` とする。
- `meta/specs/contracts.rs` に契約を足す。
- `crates/agents/src/role.rs` の `capabilities()` の全ロール共通集合に加える（決定 h、読み取り専用）。
- `crates/runtime/src/restore.rs` の `tool_may_have_side_effects` の副作用なしリストに加える。
- `scripts/check-cache-contracts.sh` の対象に加える。
- 出力上限は 16 KiB。

#### 受け入れテスト

- 別の `project_root` のスレッドは `thread_denied` になり、文言は存在しない ID の場合と同一である。
- 同じプロジェクトのスレッドは読め、子 run も含まれる。
- `host_chat` に含まれる ID なら読める。`working_note` に含まれる ID では読めない。
- escalation の子と元が相互に読める。
- thinking / reasoning / 画像が出力に出ない。secret は redact される。
- `around` で ref が安定する。`max_bytes` を超えると cursor が返る。
- 契約テスト、ツール列挙テスト、キャッシュ契約テスト（`scripts/check-cache-contracts.sh`）が通る。

### A-3 GUI

`crates/gui/src/app/thread_mirror.rs` などに `threads` ミラーを置く（前例は `usage_ledger::sync_usage_ledger`）。`sidebar.json` のスレッド定義を 1 秒ごとに `try_upsert_threads` で書く。

受け入れ: headless テストで、スレッドの追加・改名・アーカイブがミラーに追従する。既存の `usage_tab_headless` も通る。

### A-4（任意）

`evorch inspect thread <id>` と `inspect_e2e.rs` のテスト。

## Phase B の設計

Phase B は `thread_search` に本文の全文検索を足す。2026-10-25 に Phase A が `5d23cab4`（PR #351）としてマージされた後に、計画を作った。初回 plan review の M1〜M6 に加え、追加レビューの R1〜R5・S1〜S2 を以下の契約に反映した。改訂後の独立 Reviewer の合格判定はまだ得ていない。以降、この節を Phase B の正本とする。実装の途中で前提が変わった場合は、この節を更新してから実装を変える。

### 確定した決定

利用者に確認した決定は次の 4 つである。

1. **B-1 既定の kinds**: `thread_search` の既定 `kinds` は、従来のメタデータ（title / user / working_note / final_report）に `conversation`（user と assistant の text）を加えたものにする。`tool_use` と `tool_result` は明示的に指定したときだけ検索する。
2. **B-2 容量の上限**: `[storage.thread_index] max_bytes` は暫定で 512 MiB とする。切り詰め後の容量を計測し、256 MiB / 512 MiB / 1 GiB のそれぞれについて、次を ADR に記録する: 保持できる run の数（N / 全体）、最古の時刻、保留（pending）の数、oversized の数。512 MiB が明らかに不適切なら、マージ前に報告する。
3. **B-3 evicted の扱い**: 上限を引き上げても、evicted の索引を自動で作り直さない。作り直すのは `reindex-threads --include-evicted` を実行したときだけである。新しい上限は、writer が設定を取り込んだ時点で有効になる。
4. **B-4 「古い」の基準**: run の作成時刻とする。同時刻なら `run_id` の辞書順とする。

### 抽出の契約（M1）

- 抽出器は W1 が storage に新設し、snapshot の入力は `serde_json::Value` とする。storage は providers に依存しないためである。provenance と最新 note は型付きの別入力にする。W6 の計測器も同じ抽出・会計 API を呼び、独自実装を作らない。
- runtime の normalize との一致は W4 の契約テストで確認する。normalize の表示文字列をそのまま比較しない。共通の比較表現は、画像表示用 note を除外し、`is_error` を `tool_error` に分類し、同じ redaction・UTF-8 切り詰めを適用した `(ordinal, block_index, kind, text, truncated)` とする。画像 note の除外は元の構造化 content の種別で行い、同じ文字列の本物の text を削らない。ordinal / block_index は加工せず一致させ、hash だけ・text の先頭だけの比較では済ませない。
- 種類ごとの扱い:

| 種類 | 索引に入れる内容 |
|---|---|
| user / assistant の text | 全文 |
| working note | 正本は `run_ledger`。各 run の最新 `seq` を全文で入れる。空本文は block を作らない。位置は `note_seq` |
| tool_use の input | redaction してから JSON 化し、先頭 4096 B |
| 成功した tool_result | 既知の text 部分の先頭 2048 B |
| エラーの tool_result | 全文（kind は `tool_error`） |
| System、reasoning、thinking、画像、anthropic の生ブロック、compaction、未知の block | 入れない |
| 不正な JSON | その run を保留する |

- ordinal は snapshot の System 以外の全メッセージを数えた 0 始まり、block_index は元の content 配列の 0 始まりとする。コピー・画像などの除外後も両方の番号を詰めない。
- 処理の順序: 構造化入力を redaction → text 化 → 全文を再度 redaction → UTF-8 境界で切り詰め → hash → 保存。`truncated` は実際に切ったときだけ true にする。診断やイベントに secret を含めない。
- 成功した tool_result は画像を除いた既知の text 部分を改行で連結して 2048 B までとする。エラーは同じ text 抽出だが 2048 B を超えても全文を保存する。画像の代替表示文字列は本文に含めない。
- 公開 `kinds: ["tool_result"]` は内部 `tool_result` と `tool_error` の和集合とする。どちらも既定の `conversation` には含めず、tool_use も明示指定だけにする（B-1）。

### provenance の保存・更新（R1）

確認した現状（HEAD `517c980e`）と、新設する契約を区別する。

- `storage/src/run.rs:14–25` の `RunContextRecord` にメッセージ別 provenance はない。`runtime/src/restore.rs:517–540` は raw messages と checkpoints を別 JSON にして保存し、`storage/src/repo/run_context.rs` が redaction する。`writer/state.rs:410–418` の既存トランザクションは snapshot と thread binding の境界であり、sidecar / source_revision はまだない。
- continuity は `agent_loop.rs:609–618` で User メッセージとして追加され、escalation の保存経路も `restore/escalation.rs:74–105` で同じ形式を保存する。descriptor に continuity 情報はあるが、それだけで各メッセージ位置の出所を証明できるものではない。
- resume の `runtime/chat_restore.rs:274` は evidence / reference consumption 付き保存を使い、`writer/state.rs:275–282` の別分岐で snapshot を保存する。この分岐も通常 snapshot と同じ sidecar 保存境界に統合し、書き込み経路を取り残さない。
- fork は `restore/fork_seed.rs:54–86` で `context_len`（System を除いたメッセージ数）を raw 配列の終端へ変換し、完了ターンで切る。範囲内の checkpoints もコピーする。`thread_lineage.rs:45–46` は `fork_point_run_id` / `fork_point_context_len` を記録するが、snapshot revision と対応した位置マップではない。
- compaction の `AgentContext::apply_checkpoint`（`context.rs:180–188`）は raw messages を消さず checkpoint を追加する。モデル向けだけ `project` / `visible_messages` で投影する。`compaction/mod.rs:540–560` は prepared context を先に永続化し、成功後にメモリへ設置する。したがって fork 境界を compaction 後の表示窓の ordinal に再適用してはならない。

保存契約:

- 同じ SQLite DB に `run_context_provenance(run_id PRIMARY KEY, snapshot_revision, snapshot_sha256, format_version, status, reason_code, map_json)` を設ける。外部ファイルではなく snapshot の sidecar テーブルであり、現行 snapshot に対応する 1 行を持つ。`run_contexts.snapshot_revision` は run ごとの単調増加整数（新規は 1、旧 snapshot は migration 時に 0）とし、成功した snapshot 保存ごとに増やす。時刻だけでは revision を識別しない。migration で旧 snapshot を provenance 確定済みにしない。
- `map_json` は version 付き JSON object とする。各 non-System メッセージの raw_index / ordinal と各 block_index、origin（`native` / `continuity_copy` / `fork_copy`）、コピー元、fork の元境界を記録する。コピー元は tagged source とし、会話なら `(run_id, snapshot_revision, ordinal, block_index)`、note なら `(run_id, note_seq)`、凍結 history なら `(history_source_id, record_index)` とする。生成した案内・境界 block は `excluded_reason=generated_boundary` で明示する。checkpoint summary は別配列であり会話 block として抽出しない。本文・secret をマップに複製しない。
- `snapshot_sha256` は storage が redaction 後に保存する messages_json と checkpoints_json を長さ付きで連結したバイト列の SHA-256 とする。マップの位置・配列長を検証し、`snapshot_revision` と hash の両方を照合する。`format_version` は出所形式、`source_revision` は snapshot / note / provenance の索引入力更新の世代であり別物である。
- W2 の snapshot 保存コマンド 1 回を原子性の境界にする。W1 の repo API が、redaction 後 snapshot・thread binding・sidecar・`source_revision` 増分・R2 の state 遷移を**同じトランザクション**で保存する。evidence / reference consumption 付き分岐もこの API を既存 transaction 内で呼び、evidence 等とともに commit する。receipt は commit 後にだけ返す。途中障害なら全部 rollback し、prepared context / provenance も設置しない。note append / clear も note と source_revision / state を同一トランザクションで保存するが、snapshot_revision は変えない。

位置対応の更新規則:

| 経路 | W4 が作る対応と W1 / W2 の保存規則 |
|---|---|
| 通常追記・continuity | 既存マップを維持し、新規発言は native、continuity を挿入した正確な位置だけ continuity_copy とする。元 note / history source の識別子も保持する。文面一致で判定しない |
| fork / rewind seed | 読んだ元 snapshot_revision に固定し、raw prefix とマップを同じ終端で切る。コピーした全 block を fork_copy とし元の位置・元 origin も保持する。fork 境界の外に追記する native は除外しない |
| compaction / model switch | 現状の raw 履歴維持なら既存位置マップは不変、checkpoint の追加と同時保存する。生成境界の追記位置は明示する。将来 raw を置換する経路は旧位置→新位置の全対応を必須とし、不明なら確定扱いしない |
| restore / 再起動 | snapshot と sidecar を同じ read transaction で読み、revision / hash / 全位置を検証して一緒に復元する。System の追加でも raw_index を更新し non-System ordinal は維持する。安全な prefix に切る場合も同じ切断をマップへ適用する |

コピー除外は**抽出時のフィルタだけ**であり、保存 messages を削除せず ordinal / block_index を詰めない。同じ文の native な別発言は別位置として必ず残す。

- 旧データを確定扱いできるのは、(a) 対応 revision と完全な位置マップが検証できる、または (b) 当該 snapshot に結び付く構造化された生成記録から、全位置の native / copy と変換履歴を一意に証明できる場合だけとする。検証した証拠を sidecar に記録する。fork metadata があるだけ、ないだけ、本文が一致するだけ、現在の境界値だけでは足りない。
- 証明不能は sidecar `status=unknown`、索引 `blocked(reason=provenance_unknown)` とする。詳細 reason_code は `missing_sidecar` / `snapshot_mismatch` / `incomplete_map` / `legacy_unproven` / `unsupported_version`。本文を推定で除外せず run の会話索引を保留し、理由別件数を計測する。新 snapshot でも不明な既存位置を native に格上げしない。証拠による sidecar 修正は source_revision を進める。
- 受け渡し API（順序 1 で型・エラー・fixture を固定）: W1 は `SnapshotProvenance` / `SnapshotWrite` / `SnapshotBundle` と原子的 `save_snapshot_bundle`・`load_snapshot_bundle`、共通 `extract_index_input` を所有する。W4 は全生成・変換経路で対応マップを作り `SnapshotWrite` を渡す。W2 は単一 writer コマンドで W1 を呼び、receipt の snapshot_revision / source_revision を W4 へ返す。W4 の restore と W2 の抽出 job は同じ `SnapshotBundle` を読む。W2 が本文から provenance を再推定する実装は禁止する。
- この保存形式、変換規則、W1/W2/W4 の API、障害時の振る舞いを順序 1 の完了条件とし、順序 2・3 の開始条件とする。実装前の fixture に fork 後の compaction → 再起動と保存途中の障害を含める。

#### 順序 1 の Rust 共有境界（storage 担当分）

型は公開 `storage::thread_index` に置く。以下は契約の定義であり、migration / writer / job / 検索を実装済みとは扱わない。

- `SnapshotProvenance` は run / snapshot_revision / snapshot_sha256 / format_version / status / map を持つ。`ProvenanceMap.version=1`、raw メッセージ数・checkpoint 数、全 non-System メッセージと全 block の位置を保持する。`BlockOrigin` は native / continuity_copy / fork_copy（元 origin と `ForkBoundary` を含む）、`CopySource` は message / working_note / frozen_history の tagged enum。本文は複製しない。SQL sidecar の status / reason_code / map_json はこれらから W1 が分解保存し、unknown の理由は `ProvenanceUnknownReason` を使う。
- hash の framing は **u64 big-endian の messages_json バイト長 → messages_json の保存バイト列 → u64 big-endian の checkpoints_json バイト長 → checkpoints_json の保存バイト列** と固定する（SHA-256、小文字 hex）。再シリアライズ前後の同一性を仮定しない。`snapshot_sha256` を W1 の保存側・抽出器で共有する。
- `SnapshotWrite` は runtime が作る map / status、従来 record、thread binding、期待する旧 snapshot_revision を渡す。storage が redaction 後に revision / hash を割り当てる。`SnapshotRepository::{save_snapshot_bundle,load_snapshot_bundle,append_indexed_note}` が原子的保存・同一 read transaction・note append/clear の境界、`SnapshotTransaction::stage_snapshot_bundle` が evidence / reference-consumption の既存 transaction 内で使う境界。`SnapshotReceipt` は commit 後のみ公開し、障害時は prepared context を設置しない。
- `IndexInput` は Value と別入力の provenance / 最新 `RunLedgerEntry`、同一 transaction の revision / hash / checkpoint 数を受ける。Value を取得しただけでは出所の証明にならない。`extract_snapshot_bundle` は正本 JSON の parse と保存バイト hash を検証する便宜入口、`extract_index_input` が共通抽出 API。全文を持つ `IndexBlock` / `ExtractedIndexInput` と固定理由だけの `ExtractionError` を返す。不正 JSON / 既知 block の壊れた shape は run 全体を保留し、未知 block は除外する。画像のみの tool_result は block を作らない。
- `BlockLocator` / version 付き `GuardedReference`・`GuardedSource` は message と working_note を別 variant にする。`SnapshotRepository::resolve_guarded_reference` は認可後に正本を読み、`GuardedResolution::{Message,WorkingNote,StaleReference,SourceDeleted,UnsupportedVersion}` を返す契約。Message は FTS text ではなく bundle と元位置、note は最新の非空 seq だけを返す。
- `IndexState` / `IndexEvent` / `IndexRunStatus` は R2 の状態と入力を表す。`ThreadIndexRepository` はヘッダ discovery・成功/失敗 commit・evict・正本削除を所有する。`IndexCommitGuard` は lease / epoch / 有効期限・source revision / version・generation・Pending・source_present・request target の全照合を固定する。SQL 遷移表の実装・各セルの受け入れは順序 3 以降。
- `ThreadIndexControl::{submit_request,request_status,cancel_request,acquire_lease,heartbeat,release_lease,step}` が W2/W5 共通入口。`IndexTarget` / `RequestGuard` で revision・version・generation を固定し、`ReindexResult` / `RequestGapReason` / `RequestSummary` / `RequestSubmission` / `LeaseResult` で R5 の受付・終端・理由別件数と owner 動作を区別する。時計は明示的に渡す。取消・待機中断・target 処理不能を制御系 failed と混同しない。
- `IndexCapacity::capacity_status(PlannedGrowth)` と `apply_index_settings(ThreadIndexSettings)` が W2 の取り込み境界。後者の receipt は適用世代を返し、writer のコマンド順に有効になる。`IndexCapacityStatus` は Normal / IndexPaused / SourceSuspended と DB/WAL/SHM・reserve・J を返す。W3 の strict 設定型・検証は別途合流が必要。
- `AccountingPolicy` / `IndexAccountant::account_index_input` は共通会計のシグネチャ（実装・係数選定は後続）。`text_bytes` は抽出後の厳密な UTF-8 バイト数。論理 `charged_bytes = text + attribute + per_block × count + per_run + ceil(text × fts_numerator / fts_denominator)`、attribute の集計対象は型コメントに固定する。分母ゼロ・overflow は `AccountingError`。これは物理 G の見積りではない。
- fixture は抽出器の `fork_compaction_restart_fixture_keeps_native_duplicate_and_original_positions` と `snapshot_save_fault_fixture_accepts_only_complete_old_or_new_bundle` に置く。後者は各保存境界の **旧/新 bundle と混在拒否の契約 oracle** であり、DB への障害注入・rollback 検証そのものは順序 3 の担当が追加する。

### スキーマ（v25）と原子性（M3）

v25 は origin/main（`870a969b`）の時点で空いている（最新は v24）。この時点の open PR（#354 / #358 / #361 / #362）は migration を変更していない。実装時に再確認する。

- migration はテーブル・索引・トリガーだけを作る。本文は処理しない。
- `thread_index_runs`: `run_id` PK、`state`、`source_revision`、`indexed_revision`、`source_context_sha256`、`source_note_seq`、`source_provenance_version`、`extractor_version`、`accounting_version`、`source_updated_at_ns`、`retention_at_ns`（run の作成時刻）、`indexed_at_ns`、`text_bytes`、`block_count`、`charged_bytes`、`priority`、`attempt_count`、`next_attempt_at_ns`、`blocked_reason`、`last_error_code`、`source_present`。`state` は `pending` / `indexed` / `retry_wait` / `blocked` / `evicted` / `deleted`。
- `conversation_blocks`: `id` INTEGER PK、`run_id`、`source_kind`（`message` / `working_note`）、`source_key`、`ordinal`?、`block_index`?、`note_seq`?、`role`、`kind`（`text` / `working_note` / `tool_use` / `tool_result` / `tool_error`）、`tool_name`?、`truncated`、`hash`、`text`。`UNIQUE(run_id, source_kind, source_key)`。書き込みは `ON CONFLICT DO UPDATE` で行い、`INSERT OR REPLACE` は使わない。消えた key は削除する。
- `conversation_fts`: fts5、external content、trigram、`columnsize=0`。`memory_fts`（`crates/storage/src/migrations/v3.sql` の 52〜62 行付近）と同様に、トリガーで同期する。
- 制御用に `thread_index_lease`（owner、期限、fencing epoch）、`thread_index_requests`、`thread_index_request_targets`、discovery の再開位置を持つ。run ごとの `work_generation` と、索引が使用した extractor / accounting version を保存する。
- 正本保存は R1 の原子性境界に従う。「常に pending 化」ではなく、evicted / deleted / retry 等を含む以下の遷移表を正本とする。
- 索引の置換は、lease の epoch と有効期限、source_revision、work_generation、現在 state が pending、source_present、適用 version、request target が未完了かつ有効（request 起因の場合）を commit transaction 内で確認する。どれかが変われば結果を破棄する。差分・FTS・集計・state・request target 結果を同一トランザクションで更新する。evict / 削除 / 明示 reindex は generation を進め、遅延 commit を無効化する。
- source の削除は、共通 API で block・FTS・集計を消して tombstone を残す。discovery では復活させない。
- hash が一致しても、位置（ordinal、block_index、note_seq など）の更新は省かない。
- 受け入れ条件に FTS の `integrity-check`（rank=1）を含める。

### 状態遷移表（R2: イベント × 現在 state）

`P=pending`、`I=indexed`、`R=retry_wait`、`B=blocked`、`E=evicted`、`D=deleted`。「維持」は attempt_count / next_attempt_at / 理由も保持することを含む。新しい revision は正本保存または証拠更新の transaction で確定した世代、同 revision の再発見は更新ではない。行がない source の初回 discovery は P を作るが、tombstone は「行がない」と見なさない。

| イベント | P | I | R | B | E | D |
|---|---|---|---|---|---|---|
| 同 revision の通常 discovery / 再起動 | 維持 | 維持 | 維持 | 維持 | 維持 | 維持 |
| 上限引き上げ・設定再取り込み | 維持 | 維持 | 維持 | 維持 | 維持 | 維持 |
| snapshot / note / provenance の新 revision | P・試行を 0 | P・試行を 0 | P・試行を 0 | P・試行を 0 | source 情報のみ更新、E 維持 | D 維持 |
| 明示 reindex（include-evicted なし） | P・試行を 0 | P・試行を 0 | P・試行を 0 | P・試行を 0 | 維持・対象外 | 維持・削除済み |
| 明示 reindex（include-evicted あり） | P・試行を 0 | P・試行を 0 | P・試行を 0 | P・試行を 0 | P・試行を 0 | 維持・削除済み |
| extractor_version 変更 | P・試行を 0 | P・再抽出 | P・試行を 0 | P・試行を 0 | E 維持 | D 維持 |
| accounting_version のみ変更 | 新係数で処理 | 本文再抽出せず再会計 | R 維持・会計 version 更新 | B 維持・会計 version 更新 | E 維持・対象 version 更新のみ | D 維持 |
| retry 時刻到達 | 維持 | 維持 | P・試行数を維持 | 維持 | 維持 | 維持 |
| 有効な抽出結果の commit | I | 拒否 | 拒否 | 拒否 | 拒否 | 拒否 |
| 抽出の一時失敗 | 試行 +1、5 未満なら R、5 回目は B | 適用なし | 適用なし | 適用なし | 適用なし | 適用なし |
| provenance 不明などの確定的処理不能 | B・理由保存 | 適用なし | 適用なし | 維持 | 維持 | 維持 |
| 容量不足 / 索引停止 | P 維持・試行を増やさない | 維持 | 維持 | 維持 | 維持 | 維持 |
| 論理 eviction / oversized / 物理回収の対象 | E・索引削除 | E・索引削除 | E・索引削除 | E・索引削除 | 維持 | 維持 |
| 正本削除 | D | D | D | D | D | 維持 |

- D 以外では新 source 情報（revision、hash、note_seq、provenance version、更新時刻、存在情報）を保存するが、E は通常 discovery・source 更新・version 変更・上限引き上げのどれでも P にしない（B-3）。D は新しい正本らしき行を見つけても source_present=false と tombstone を維持し、`source_after_tombstone` を記録する。同 run_id の再利用は禁止する。
- P 化する契機は generation を増やし旧 job を失効させる。同 revision の R / B を discovery しただけでは generation / attempt_count をリセットしない。明示 reindex は新しい処理要求としてリセットする。provenance_unknown は再試行しても証拠がなければ B に戻す。
- 失敗・blocked の保存にも成功 commit と同じ epoch / revision / generation / state / request guard を適用し、旧 job の失敗で新 revision の試行数や state を変えない。
- extractor 変更は位置・本文が変わり得るため非 E / D を再抽出する。accounting のみの変更は保存済み text_bytes / 属性 / block_count から新係数で再会計し、version・集計・必要な論理 eviction を同一トランザクションで確定する。本文検索を不必要に再抽出しない。再会計中の旧会計値は楽観的に解放せず、完了するまで索引増加を止める。
- evict と D 化は block / FTS / 集計を同時に削除する。D では source_present=false とする。discovery と遅延 commit の両方を tombstone / generation で拒否する。

### job（M2）

- 起動時の discovery: `run_contexts` などのヘッダを keyset で走査し、本文は読まない。状態変更は R2 に従い、R / B の試行数・E / D を保存する。新しい run から処理する。
- アイドル時を優先する。ただし、通常コマンドが 32 件、または 100 ms 経過したら、必ず 1 step を実行する（`writer/state.rs:390–396` の `run_due_work` に組み込む）。
- 抽出は writer の外で 1 並列で行う。同じ read transaction の SnapshotBundle を入力にし、commit 時は上記の epoch / revision / state / generation / request の全条件を再確認する。
- pending がない、無効、容量不足、retry 待ちのときは、10 ms ごとに writer を起こさない。
- retry: 1 秒から始め、最大 5 分の指数 backoff とする。同じ revision で 5 回失敗したら blocked にし、後続の run は進める。
- 巨大な run が前景の書き込みを待たせる時間を計測する。

### 上限と容量（M4）

- `max_bytes` は論理会計に対する予算である。論理会計は、text のバイト数に、属性と索引のコスト、FTS の係数を加えたものとする。`accounting_version` を持たせ、係数は計測で決める。
- 物理サイズ（dbstat、freelist、DB と WAL）は別途計測する。
- 超過したら `retention_at_ns ASC, run_id ASC` の順に evict する。古い run の backfill のために、新しい索引を evict しない。
- 1 つの run だけで上限を超えるときは `evicted(oversized)` とする。

物理 reserve と admission（R3）:

- `H=hard_limits.max_db_bytes`、`P=file_sizes().total()`（現状 `storage/src/db.rs:114–124` では DB+WAL+SHM）、`F=計測で得る前景正本 transaction の増加量の保守的上界` とする。snapshot / note / event のうち最大を F とし、`R=max(R_floor, ceil(safety_factor × F))` を正本用物理 reserve とする。`R_floor>0`、`safety_factor>=1`、再開用ヒステリシス `J>0`、書き込み 1 step の上限は順序 2 の計測で数値案を得て、**順序 5 の開始前**に W1/W2/W6 が ADR に値・根拠を記録する。実測最大は普遍的上界と断定せず、上界外入力は索引を止めて再評価する。
- 各索引 commit の直前、writer 内で他書き込みと直列化し、SQLite の write transaction を確保して P を再計測する（別プロセスの writer とも競合させない）。予定差分の text / 属性 / FTS / page 丸めと WAL frame の増加（旧 page の書き換えも含む）から `G_db` / `G_wal` / `G_shm` を保守的に見積もり、`G=G_db+G_wal+G_shm` とする。削除予定の論理バイトを物理空きとして先取りしない。安全な増加上界を算出できなければ `growth_unbounded` として admission を拒否する。論理会計係数を物理増加量そのものと見なさない。
- `P+G+R >= H` または `WAL+G_wal >= hard_limits.max_wal_bytes` で索引の書き込みを停止する。境界の等号も停止側とする。再計測で `P+G+R+J < H` かつ `WAL+G_wal+J < hard_limits.max_wal_bytes`、正本書き込み停止が解除されたときだけ再開する。H が reserve を満たせなければ索引無効相当とし正本を優先する。上限変更は writer の設定取り込み時から判定に使う。
- reserve / J は索引への admission 制限であって正本の停止閾値ではない。`IndexPaused` 中も snapshot / note / event は残る物理余裕を利用でき、正本の停止は共通判定の hard limit 側で決める。索引停止をそのまま writes_suspended に伝播しない。
- 共通 `capacity_status(planned_growth)` API（W2 所有）は `Normal` / `IndexPaused(reason)` / `SourceSuspended(reason)` と DB/WAL 実測・reserve を返す。snapshot / note / event と索引 job はこの容量圧迫判定に接続する。現状 `writer/state.rs:190–207` は容量停止中の snapshot / note に `StorageError::Serialization` を返すため、エラー型のパターン一致や文字列解析を回収開始条件にしない。共通判定と容量変化通知で接続し、正本 API のエラー型変更は本設計の前提にしない。
- **論理予算 eviction** は max_bytes を守るための索引削除であり、即座のファイル縮小を保証しない。**物理回収**は共通判定の DbSize 圧迫（reserve 不足による予防停止を含む）だけで開始し、他の LimitKind では行わない。論理予算に空きがあっても物理 admission を省略しない。
- 1 圧迫イベントにつき最大 2 サイクル（予算付き索引削除 → checkpoint → 可能なら incremental vacuum → DB+WAL 再計測）。削除・回収自身の WAL 増加にも admission を適用し、正本 reserve を使い切らない。`auto_vacuum=NONE` では incremental vacuum の効果を期待せず、full VACUUM は自動実行しない。
- 2 サイクル後も再開条件未達なら `reclaim_exhausted`、reader が WAL を保持しているなら `wal_reader_pinned` を永続的な停止理由として記録し、索引作成を止める。短周期で回収を再試行しない。reader 解放 / 容量設定変更 / cleanup による実測空き増加の通知後に再判定し、条件を満たした場合だけ停止を解除する。P / R / G / 回収前後を診断に残すが本文は残さない。

### runtime の検索（M5）

- kinds は、既存の title / user / working_note / final_report に、`conversation` / `tool_use` / `tool_result` を加える。既定は決定 B-1 のとおり。同じ位置の複数の一致は 1 つにまとめる。
- 既存の意味を保つ: query 配列は OR、`words` / `phrase` / `regex`、`include_children`、`updated_after`。
- visibility を先に確定し、scoped relation か keyset batch で storage の API に問い合わせる。100 はバッチの上限であり、総数の上限ではない。「FTS を先に引いてから可視性で絞る」方式（S2）は使わない。
- 3 scalar 未満の語や、Unicode で一致が保証されない場合は、`LIKE`（エスケープしてバインド）か scan を使う。regex は可視範囲を走査する。予算を超えたら `partial` と続きの情報を返す。
- cursor は version 付きで、caller、scope、正規化した条件、keyset の位置、regex の位置、revision、改ざん検出を含む。毎回、再認可する。run が 101 件以上あるスレッドでも続きを取れる。
- `source_revision` が一致しない古い索引は、hit にも件数にも出さない。
- hit に `truncated` と source_revision を付ける。guarded reference は version と source_kind を持つ tagged locator とし、毎回再認可する。従来の unguarded ref は従来どおり使える。
- 会話の locator は `(run_id, snapshot_revision, source_revision, ordinal, block_index)` とする。`thread_read view=messages` は現状も正本 snapshot を normalize する経路（`read.rs:466–477`）なので、FTS の text を返す経路に変えない。同一 read transaction で revision と位置を検証し、異なれば `stale_reference`、一致すれば元 snapshot の該当 block を従来の表示・応答上限で返す。
- working_note の locator は `(run_id, source_revision, note_seq)` とし、会話の ordinal / block に偽装しない。W4 が `thread_read` の guarded note 解決分岐を追加し、run_ledger の**最新** seq が一致し本文が空でない場合だけ note と seq を返す。snapshot の messages から note を探さない。更新・clear 後は旧 note を再表示せず `stale_reference`、source 削除後は `source_deleted` とする。clear は空本文の新 seq を append して source_revision を進める。
- 統計は可視範囲だけを対象にする: indexed / pending（retry と blocked を含む）/ evicted / missing。`unindexed = pending + evicted + missing`。`backfill_in_progress` も返す。
- `thread_grep` は正本の走査のままにする（索引に依存しない）。
- tool description を更新し、`crates/runtime/tests/thread_tools_cache.rs` のフィクスチャを更新する。

### CLI と所有権（M6）

- `evorch reindex-threads [--db] [--project] [--since] [--include-evicted] [--wait]`
- GUI と CLI は、共通の request / lease / step API を使う。CLI 専用の更新処理は作らない。
- DB 単位の lease と fencing epoch、heartbeat、期限切れ lease の引き継ぎを持つ。
- `--include-evicted` でも、上限は守る（決定 B-3）。

request 契約（R5、順序 1 で固定）:

| 受付時の有効な GUI owner | `--wait` | 動作・応答・継続主体 |
|---|---|---|
| あり | なし | request を永続化し `accepted` と request_id を返す。GUI owner が共通 step で継続 |
| あり | あり | request を永続化し CLI が terminal 結果まで待機。GUI owner が処理し、CLI は二重に実行しない |
| なし | なし | CLI が lease を取り同じ step を foreground で terminal まで進め、結果を返す。早期終了して処理主体を失う accepted-only にはしない |
| なし | あり | 同じく CLI が owner として terminal まで実行・待機する |

「owner なし」は他の CLI owner もいない場合とする。別 CLI が有効 lease を持つ場合は GUI owner ありと同じ扱いにする。受付と lease 取得の競合は fencing 付き API で解決する。

- `submit_request` transaction が受付時の `--project` / `--since` 条件に合う source ヘッダから対象 `(run_id, source_revision, extractor_version, accounting_version)` を `thread_index_request_targets` に物理化し、対象集合を固定する。since は source_updated_at_ns の下限（以上）、project は canonical project 一致とする。E は include-evicted 時だけ含む。D は対象外として削除済み件数を報告し、固定後に削除された target は gap とする。0 件なら completed。
- 受付後の新 run は追加しない。request の terminal 化前に target の source が更新されたら旧 snapshot の再現を要求せず `superseded` gap に確定し、新 revision は通常 job に任せる。処理済み target も未完了には戻さず、終了 transaction の再検証で成功か gap を確定する。version が変更された場合も `version_superseded` gap とし、固定 target の内容を書き換えない。terminal request の結果はその後の source 更新では変えない。
- target は新規に処理するか、同 revision / version の既存索引 commit を確認すれば成功にできる。B / R の明示再試行と E の復帰は R2 に従う。`request_id + target + work_generation` を有効性 token に含め、取消済み・terminal・superseded target 向けの遅延 commit を拒否する。通常 job の成功を同じ target の完了に利用してよいが、上記照合を省かない。
- 複数 request が同じ run / revision / version を対象にするときは work を共有する。明示 reindex で generation を進めるなら、有効な全 target の generation 対応も同じ transaction で更新し、旧 job を拒否する。1 request の取消を他 request の取消と見なさない。

| request 結果 | 条件と理由の扱い |
|---|---|
| accepted | request が永続化され、別 owner に処理を委ねた非待機応答だけ。完了を意味しない |
| completed | 固定 target がすべて同 revision / version で成功（または対象 0 件） |
| completed_with_gaps | 全 target が成功または理由付き終端に到達し、gap が 1 件以上。blocked（provenance_unknown / 5 回失敗等）、capacity_paused、oversized、evicted_by_budget、固定後の deleted、superseded を gap として件数・理由を返す。全件 gap でも failed とはしない |
| failed | 不正引数、DB 利用不能、request/lease 永続化不能、回復不能な制御処理障害。target の通常の処理不能とは分ける。受付前なら request_id なし、受付後は failed を記録する |

- request は容量不足の解消を無期限には待たず、物理 admission が拒否した target を `capacity_paused` gap にする。通常の P は残し、容量通知で job が再開しても terminal request の結果は変更しない。oversized は E、blocked は B、削除済みは D を維持する。別 target の論理 eviction が起きた場合は request 終了時にも保持 revision を再確認し、未保持を evicted_by_budget gap にする。
- 待機中に owner が異常終了した場合、request / target / 試行数は DB に残す。待機 CLI は lease 期限到達後に CAS で引き継ぎ候補となり、勝者（新 GUI または CLI）が新 epoch で未完了 target から再開する。旧 epoch の commit は拒否し、結果を二重計上しない。待機 CLI が停止しても request は取消さず、次の GUI/CLI owner が継続する。
- accepted-only 応答後に唯一の owner が落ち、待機 CLI もいなければ、隠れた daemon は起動しない。request は永続的に待機し、次回 owner 起動時に再開する。CLI は request_id と owner 不在状態を診断で確認できる共通 status API を持つ。待機の中断は request の failed / completed と混同しない。

### 担当（ファイル所有）と順序

共有ファイルも単独 owner を置く。別担当は API / fixture の提案を渡し、同じファイルを同時編集しない（S1）。

| 単独 owner | ファイル / 責務 |
|---|---|
| W1 storage-core | `crates/storage/src/{run.rs,thread_index.rs,thread_index/*,read.rs,lib.rs}`、`migrations/*`、`repo/{thread_index.rs,run_context.rs,run_ledger.rs,mod.rs}`。provenance 型・schema・抽出/会計・状態 SQL・request 永続化 API。storage の `read.rs`（同一 read transaction の bundle / guarded reference 読み取り配線）も W1 の単独所有 |
| W2 writer | `crates/storage/src/{writer.rs,writer/state.rs}` と新設 job module。writer コマンド、容量 API、lease/step、設定取り込み。storage 内 wiring は W2 だけが編集 |
| W3 config | `crates/storage/src/config.rs` と `crates/config/src/*` の設定型・strict・既定値・保存。設定値と適用時点の API は順序 1、実配線は順序 7 |
| W4 runtime | `crates/runtime/src/meta/threads/*`（`read.rs` / `normalize.rs` 含む）、`meta/specs.rs`、`agent_loop.rs`、`context.rs`、`continuity.rs`、`restore.rs` / `restore/*`、`runtime/chat_restore.rs`、`thread_lineage.rs`、`compaction/*` と runtime の契約テスト。生成側 provenance と guarded note 解決 |
| W5 CLI / app wiring | `crates/evorch/src/{main.rs,reindex_threads.rs,lib.rs}`、実際の GUI storage 起動経路 `crates/gui/src/bin/evorch-gui.rs:661–667`、GUI の `app/{state.rs,storage_settings.rs}`。W3 の設定型→W2 の取り込み API の配線も W5 が単独所有する。別配線ファイルが必要なら順序 1 で表に追記してから着手 |
| W6 計測 / ADR | 計測ハーネスと本 ADR。W1 の `extract_index_input` / 共通会計関数を呼び、別の抽出器・係数は作らない |

順序（チェックリストの 1〜9 に対応）:

1. 契約の固定 — R1 の保存形式・revision・全変換経路・W1/W2/W4 API と fixture、R2 の遷移・commit guard、R3 の容量 API / 算出式、R4 の比較表現 / locator、R5 の request / owner 動作表、設定型・API・単独 owner を固定する。未決事項が残れば完了にしない
2. 最小の抽出器で計測 — 順序 1 完了が開始条件。W1 の抽出器と W6 の呼び出しを先に作り、索引化可能な母集団を確定。順序 3 の provenance 保存実装が済むまでは検証済み fixture / 証明可能な旧データだけで測り、不明データを native と仮定しない
3. migration・抽出・状態遷移 — 順序 1 完了が開始条件。R1 の原子的保存 / restore と R2 の遷移を実装し障害注入で検証
4. 不可分な置換
5. job・lease・上限 — 開始前に順序 2 の実測から物理 reserve / G の上界係数 / J / step 上限 / 前景待ち許容値を ADR に記録する。裏付け不足なら順序 2 に戻り、値未決のまま開始しない
6. 検索・cursor・ref
7. CLI・設定・spec の配線 — 順序 1 で固定した型・request 契約を配線する（この段階まで API 設計を先送りしない）
8. 実 store のコピーで検証
9. ADR の仕上げ

### テスト方針

固定の sleep は使わない。時計を注入し、step を明示的に進め、commit 通知・barrier・channel で待つ。

- 抽出: 境界値（4096 B と 2048 B の手前・ちょうど・超え）、UTF-8、切り詰め境界をまたぐ secret、古いデータの secret。除外、同じ文の別発言、note の変更と clear。
- 索引: 差分（追加・更新・削除、compaction）、障害注入と rollback、再起動、FTS の integrity-check、source の削除と tombstone。
- job と容量: コマンドが続く間の進行、busy-loop しないこと、retry、容量の境界、巨大な run、逆転しないこと、reader が WAL を回収できない場合。
- 検索: run が 101 件以上のスレッド、短い語、`LIKE` のエスケープ、Unicode、regex の続き、16 KiB での打ち切り。
- cursor: 改ざんと条件の変更。不可視の run を大量に足しても、応答（件数・統計・partial・cursor）が変わらないこと。guarded ref が stale になること。
- 所有権: GUI と CLI の同時起動、二重起動、異常終了、lease を奪った後に古い owner の結果が commit されないこと。
- CLI: `--project` / `--since` / `--include-evicted`、accepted と completed の区別。
- 回帰: Phase A の回帰、grep が索引に依存しないこと、spec のバイト一致。

追加レビューの必須受け入れケース（実装時に結果をチェックリストへ記録）:

- R1: 同文の copy / native を含む fork → compaction → 再起動でも native な別発言を誤って除外しない。snapshot 保存の各境界に障害を注入し、旧 snapshot+旧 sidecar か新 snapshot+新 sidecar だけが見え、部分保存で位置がずれない。restore / safe prefix / System 追加でも ordinal が合う。不明な旧データは理由付き B になる。
- R2: evict → 上限引き上げ → snapshot/note 更新 → 再起動でも E のまま。blocked 後の再起動・同 revision discovery で attempt_count がリセットされない。version 変更 / 明示 reindex の例外、削除と遅延 commit の競合、request 取消・世代変更の commit 拒否を表の各セルに対応させる。
- R3: 論理予算に空きがあるが物理 reserve 不足なら索引 commit を止める。停止後にも reserve 内の snapshot/note 正本書き込み余裕を維持できる。P+G+R の手前・ちょうど・超え、2 サイクル不成功、reader の WAL 保持、auto_vacuum=NONE、再開ヒステリシスを検証する。
- R4: 画像+text の tool_result、2048 B 超の error、明示 tool_result 検索で成功/エラーの両方が出て既定 conversation には出ないこと。note 更新・clear 前後の guarded ref が snapshot の同 ordinal の発言へ誤解決されず stale になること。
- R5: GUI owner あり/なし × wait あり/なしの 4 ケース、受付後の source 更新/削除、blocked / 容量不足 / oversized / 全件 gap / 0 件の結果、待機中の owner 異常終了→lease 引き継ぎ、accepted 後の owner 不在→次回起動で継続を検証する。

### 計測計画

実 store `/home/turtton/.config/evorch/evorch-events.db` は `mode=ro` で開き、backup API でコピーしたものを使う。原本には書き込まない。

- 切り詰め後の本文（kind 別）、FTS、補助テーブル、WAL のピーク、回収後のサイズ、上限ごとの保持範囲、`provenance_unknown` の理由別件数。各 256 MiB / 512 MiB / 1 GiB で N/全体に加え、索引化可能 run 数（完全な provenance・既知形式）、その対象本文量、保持本文量も併記する。不明な旧データを母集団から黙って落とさない。
- 確実に hit が存在する固定 fixture と、実 store コピー内で正本走査により hit を確認した dataset の両方を用いる。短語（1/2 scalar）、3 scalar 以上、Unicode、phrase、regex、明示 tool kinds、no-hit を分け、正解 hit 集合を記録する。no-hit だけの速度を合格根拠にしない。
- 検索 p95 < 300 ms: release ビルド、warm / cold、backfill あり / なし、語の種類別。計測と本番は同一の抽出器・extractor_version / accounting_version・会計係数を用いる。
- 前景の書き込み待ち: 同じ workload / store / 設定で索引無効時を基準に、backfill / 巨大 run / 回収時の p50 / p95 / 最大と増分を測る。順序 2 で基準値と許容する絶対増分・倍率を数値化し、順序 5 前に ADR に固定する（未計測・現時点では未検証）。超過したら step / transaction の分割・予算縮小を行い再計測し、それでも超過なら実装を止め利用者へ判断を求める。基準を後から緩めて合格扱いしない。
- R3 の F / G / reserve / 停止再開閾値は DB+WAL のピークから検証する。論理 bytes と物理増加量は別々に報告する。
- 結果は「Phase B 実装状況チェックリスト」の計測の項目に記録する。

### 実装時に確認すること

実装直前に再確認する項目: v25 の空き、source を更新・削除する全経路の棚卸し、note しかない run の binding、既存の排他の再利用可否、bundled SQLite の trigram / dbstat、DB 全体の上限設定の出所と reload / 再起動の実配線、cursor の互換性。

HEAD `517c980e` で確認済みの現状は R1（provenance 不在・continuity / fork / compaction / restore の保存）と R3（容量停止エラー・DB/WAL/SHM 集計）に記録した。sidecar 保存先・位置対応・新設定の有効時点は上の新設契約で固定した。v25 の空きは起点時の確認であり、実装時に取り直す。reserve と前景許容値の数値・契約の実装成立性は未計測 / 未検証で、順序 5 の開始ゲートを省略しない。

## Alternatives

- ID の形式が正しければ許可する: 推測可能な ID による越境閲覧を招く。
- 全文を常時、索引化する: 初期段階でストレージと更新のコストを負い、需要を確認できない。Phase B で計測を見て決める。
- モデルに SQL を直接実行させる: 認可・secret の除去・出力量の制御を迂回する。
- `history_read` に他スレッドの source を足す: `history_read` は自分の run の履歴と明示的な引き継ぎ用で、スレッド単位の権限・検索・要約の要件が異なる。権限モデルが混ざるので分ける。
- `run_output` / `wait` を escalation 先にも広げる: 影響範囲が大きく、既存の挙動を変える。thread ツールだけに限る。
- Phase A からポインタ注入する: 参照の需要と権限の運用を確かめてからにする（Phase C）。

## Consequences

- 全ロールが他スレッドの内容を読めるようになるので、権限判定（visibility）の誤りは会話の漏えいになる。判定を `visibility` に集約し、拒否の文言を 1 つにして、受け入れテストで固定する。
- `threads` に host と runtime の 2 つの書き手がいる。列ごとの `COALESCE` と `source` で衝突を抑えるが、`sidebar.json` と DB の不整合は起こりうる。`thread_read` の overview に不整合の注記を出す。
- 会話の中の文字列は信頼できない入力である。出力に untrusted の注記を付けても、モデルが指示として従わないことは保証しない。
- Phase A は本文の索引を持たないので、`thread_grep` の線形走査は大きなスレッドで遅くなりうる。Phase 0 の計測と、Phase A 後の利用実績を見て Phase B を決める。

## 計測と検証の限界

既知の制限: `thread_read` の root / child run の終了理由は、live runtime に登録中の run についてのみ返す。保存済みデータだけの run は終了理由を返さず、保存された phase を返す。これは storage の `RunTerminalSummary` が終了理由を保持しないためであり、tool description にも明記する。

Phase 0 の計測は完了した。正規化時間は Python の JSON parse・テキスト化による参考値であり、Rust 実装性能を示すものではない。FTS の全量サイズは 10% サンプルの SQLite DB ファイル実測値を単純に 10 倍した外挿であり、SQLite の管理 overhead を含むサンプル値から全量索引の実サイズを保証するものではない。

## 実装状況

各項目の完了時に `[x]` にして、commit SHA を記入する。

- **2026-10-25 に origin/main を merge で取り込んだ（force push を避けるため）。** ユーザーの許可を得て一度 origin/main（ceaa9501）へ rebase したが、rebase 後の force push は権限レビューで認められなかった（1 回目は承認待ちのタイムアウト、2 回目と 3 回目は「force push は実行前にユーザーの明示的なレビューが必要」として拒否）。そこで Orchestrator の指示により、push 済みの履歴は書き換えずに merge コミット 091442b1 で origin/main（d3900116。#341 まで）を取り込み、fast-forward で push した。ブランチの起点は 86d5b332 のままで、下の各項目の SHA も元の値である。
  - 衝突は 3 ファイルで、どれも双方の追加を残して解消した。`crates/storage/src/repo/run_context.rs` は A-1 の `from_row` と #337 の `compare_and_swap_restore_state` を両方残した。`crates/runtime/src/restore.rs` と `crates/runtime/src/restore/escalation.rs` は、`RunRestoreDescriptor` の #337 の `admission_reservation_id` と A-2 の `thread_id` を両方残した。
  - #337 の予約解除（`SnapshotReservation`）は descriptor を複製して `config_json` に書き戻すので、`thread_id` は変わらず、`thread_runs` と食い違わない。
  - origin/main は v24 を使っていない（最新は v23）ため、マイグレーション番号は v24 のままとした。
  - rebase した結果は、ローカルのブランチ `feat/thread-context-ref-rebased-local`（ea269e8e、push していない）に残してある。コードは merge 後と同じで、差は「その後に origin/main に入った #341 の `crates/gui/tests/terminal_headless.rs`」と「ADR の記載」だけである。
  - PR 作成前に `git merge-tree` で origin/main との衝突がないことを再確認する。
- [x] ADR 作成 — commit: （この ADR を追加したコミット。`git log -- intents/evorch/decisions/0041-thread-context-reference.md` で確認）
- [x] Phase 0 計測 — commit: 計測結果を記入したコミット（`git log -- intents/evorch/decisions/0041-thread-context-reference.md` で確認）
- [x] A-1 storage — commit: 35021ada
- [x] A-2 runtime — commit: 11d6a7ae（系譜の記録）, 20f18190（visibility / normalize）, 2dfdabcc（3 ツールと登録）, ee488e59（history_read の案内）, 25b4b6a6（受け入れテストとキャッシュ契約テスト）
  - ADR に記載のない実装判断: 3 ツールを `agent_loop/tool_calls/ledger_events.rs` の `VISIBLE_META_OPS` に加え、GUI の会話にツール呼び出しとして表示する（他スレッドを読んだことを利用者が見られるようにするため。Orchestrator 承認済み）。`thread_grep` / `thread_read` の `thread` 引数は 256 バイト以下とし、超えると存在に依存しない `thread_args_invalid` で拒否する（拒否文が ID をそのまま返すので、出力上限内に収めるため）。
  - キャッシュ契約テストは `crates/runtime/tests/thread_tools_cache.rs`（`scripts/contract-test-filters.sh` の `binary(=thread_tools_cache)`）。
  - 保存済み run の終了理由は A-1 の読み取り API（`RunTerminalSummary`）に含まれないため、`thread_read` の終了理由は runtime に登録中の run についてのみ返す（それ以外は phase だけ）。
- [x] A-3 GUI — commit: 00208ac4（会話のツール見出し）, e544db44（`sidebar.json` → `threads` のミラー）, 58997357（解決できない `project_root` を送らない）, cd246a11（rewind・版の切り替え・escalation の追従テスト）
  - `crates/gui/src/app/thread_mirror.rs` の `sync_thread_mirror` が、`SYNC_INTERVAL`（1 秒）ごとに前回から変わった定義だけを `try_upsert_threads` で書く。`frame.rs` の `logic` と `update` の両方から呼ぶので、ウィンドウが隠れている間も追従する。
  - 受け入れテストは `crates/gui/tests/thread_mirror_headless.rs`（4 件）。スレッドの追加・改名・fork・アーカイブと復元、symlink 経由の `project_root` の canonical 化と runtime が書いた値の保持、rewind と版の切り替え、escalation の追従を確かめる。既存の `usage_tab_headless` も通る。
  - ADR に記載のない実装判断: sidebar が知らない値は `None` で送り、保存済みの値を消さない。`project_root` を canonical にできない（ディレクトリがない）間は送らず、canonical でないパスを可視性の鍵にしない。`escalation_source_run_id` があるスレッドの `lineage_kind` は fork / rewind より escalation を優先する。3 ツールの呼び出しは会話に見出し（Search threads / Grep thread / Read thread）つきで表示する。
  - 同じ 4 コミットは `feat/thread-context-mirror`（4cd7ce9d, 3fec56b2, 5719f60b, f97b50fc）にもある。`git range-diff` でパッチが同一であることを確認した。
- [ ] A-4 inspect（任意）— 任意のため今回は見送り
- [x] 検証 — 対象: 091442b1（merge 後の HEAD）。記録: この結果を書いたコミット。テストは `TMPDIR=/tmp/evt` で実行した（理由は下記の環境起因の失敗）。
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`: 成功（警告なし）。
  - `cargo fmt --all --check`: 成功（差分なし）。
  - `scripts/check-cache-contracts.sh -j 4`: 263 件すべて成功（`thread_tools_cache` の 2 件を含む）。
  - `cargo nextest run -p storage -p agents -p gui -p runtime --locked -j 4`: 3,523 件すべて成功（42 件 skip）。`thread_mirror_headless` 4 件、`thread_mirror` の単体テスト 4 件、`usage_tab_headless` 4 件（ignore の 1 件を除く）、`terminal_headless` 8 件を含む。
  - 依存 crate: `cargo nextest run -p evorch -p eval -p recorder --locked -j 4` で 124 件すべて成功。
  - 環境起因の失敗: rebase した結果での最初の実行（sandbox の長い `TMPDIR`）では、上の 4 crate のうち 4 件が失敗し 1 件がタイムアウトした。3 件は `path must be shorter than SUN_LEN`（Unix ソケットのパスが長すぎる）で失敗した。残りの 2 件（ownership IPC を使う `headless_layout` の 1 件と、端末の `terminal_headless` の 1 件）も `TMPDIR=/tmp/evt` では成功したため、同じソケットパス長の問題と判断した。
  - コード起因の失敗はなかった。
- [x] 受け入れレビュー — 独立 Reviewer 合格（対象 HEAD: `51c6ba50`、must / should / nit なし）
- [x] PR 作成とマージ（ユーザー許可: CI 成功と独立レビュー合格後に squash マージ）— PR: [#351](https://github.com/turtton/evorch/pull/351)、PR 作成時 head: `e7b79534`。squash マージ済み。merge commit: `5d23cab4`（`feat: reference other threads' conversations with thread_search, thread_grep and thread_read (ADR 0041) (#351)`、2026-10-25 04:51:57 +09:00）。

### 受け入れレビュー 1 回目の指摘への対応

独立レビューの初回判定は不合格（対象 `bed2cc25`）。以下を修正済み。独立した再レビューの合格判定はこの時点では未取得だった。受け入れレビュー 3 回目で合格した（下記参照）。

- **M1** — `4abdd2e3` (`fix(runtime): preserve complete trusted thread authorization input`): 認可に使う `trusted_thread_request` と pending escalation の継続リクエストは完全な人間入力を保持し、goal 表示用の切り詰めと分離。8,192 / 4,096 / 4,064 文字の境界で `thread-10` が `thread-1` の認可にならないことを確認。
  - 補完 `0d729902` (`fix(runtime): keep goal display bounded beside trusted input`): goal 作成時の表示欄だけ従来の 8,192 文字上限を保ち、全文認可を維持しつつ goal の復元が成功する回帰テストを追加。
- **M2** — `c34339a7` (`fix(runtime): redact all thread metadata and separate search cursors`): JSON のキー・値を含め全出力文字列を `SecretRedactor` に通す。認可・比較用の生値はそのまま保ち、検索 cursor は ID 平文を含まない timestamp / DB rowid の位置トークンに分離（可視性は毎回再判定）。`ghp_` + 40 字の credential 形状を project ID / canonical root / thread ID に含め、非漏えい・認可不変・cursor 継続を検証。
- **M3** — `7ef3aead` (`fix(runtime): bound thread tool headers and enforce cursor progress`): `tool_name` / `call_id` の表示を 80 文字以下（省略記号つき）とする。正規化の照合・反復折り畳み用の値は切らない。空ページで同じ cursor を返さず、予算内に収まらない場合は明示エラー。18,000 バイトの ToolUse ID / name で read（1,024 バイト予算）/ grep が有限回のページングで後続へ到達することを検証。
- **M4** — `ce4c91ca` (`fix(runtime): cap and redact thread tool preflight errors`): handler の parse / エラーと agent loop の preflight 拒否を thread ツール専用に redact 後 16 KiB 以下に制限。汎用 schema 検証および `crates/tools` は変更しない。約 30 KiB の未知フィールド名・不正 enum 値を実 agent loop へ渡し、ツール結果・完了イベントの上限と非漏えい、他ツール不変を検証。
- **S1** — `2b58604a` (`fix(runtime): apply thread search cursors before project scans`): storage 取得開始位置に cursor を反映。1 呼び出し 100 候補の走査上限と全体の検索範囲を分離し、先読み 1 件で継続を判断する。2,106 スレッドの全件列挙・最古ヒットへの到達・同一更新時刻の ID 順を検証。
- **N1 / N2** — `95fae8f9` (`docs(runtime): clarify thread terminal reason limits and ADR status`): tool description に終了理由は live 登録中のみと明記し契約テストで固定。この ADR の既知の制限と Status（A-1〜A-3 実装済み、A-4 見送り）を更新。
- 最終検証（コード最終 `0d729902`、ADR 記録を含む対象 HEAD `91633635`）: 権限昇格し、`CARGO_TARGET_DIR=$PWD/target`（この worktree のみ）、`CARGO_BUILD_JOBS=4`、`TMPDIR=/tmp/evt`（`mkdir -p` 済み）で実行。
  - `cargo nextest run -p storage -p runtime -p agents -p gui --locked -j 4`: 3,529 件すべて成功、42 件 skip（nextest run `f913dcec-c390-4eb2-a104-a5f1ebccde73`）。
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`: 成功、警告なし。
  - `cargo fmt --all --check`: 成功、差分なし。
  - `scripts/check-cache-contracts.sh -j 4`: 263 件すべて成功（`thread_tools_cache` 2 件を含む。nextest run `9856e61a-4eec-45fd-ae34-3519c36cf83f`）。
  - 今回の検証ではコード起因・環境起因の失敗ともになし。`crates/tools` は変更していない。実 store は使用していない。
  - 修正 7 コミットおよび ADR 対応記録 `91633635` の 8 件は GitHub API の `commit.verification` で `verified=true, reason=valid` を確認済み。全 push は gh auth token を使った HTTPS、通常の fast-forward（force / ssh / フック迂回なし）。
- 区切りごとの検証: M1 関連 35 件、M2 thread 関連 23 件、M3 24 件、M4 25 件、S1 26 件、description 契約 5 件、M1 補完 goal 関連 25 件がすべて成功。各コミットの fmt / pre-push clippy フックも成功。署名付きコミットは各回通常の fast-forward push を実施。

### 受け入れレビュー 2 回目の指摘への対応

2 回目の独立レビューも不合格（対象 `9cfa08c0`、must 2 件: R1 / R2）。その修正は以下のとおり。3 回目の独立レビューは合格（対象 `51c6ba50`、must / should / nit なし）。したがって受け入れレビューは完了。

- **R1** — `f0d0c207` (`fix(runtime): reject incomplete legacy handoff authorization`): `PendingEscalation` に `trusted_request_complete` を追加。完全な人間入力を保持する新 writer のみ true とし、serde default は false。旧形式・マーカーなしの保存値は `trusted_thread_request` の認可証拠へ戻さない。follow-up がある場合は、旧保存値を混ぜず完全な新入力だけを認可証拠として保存する。新入力なしでは単なる再保存で完全性を昇格させない。
  - 回帰テスト: 旧形式の 8,192 文字切り詰め済み pending escalation と新形式の全文を保存し、fresh runtime で新入力なし／follow-up ありの 4 通りを復元。別プロジェクトの `thread-1` が許可されないこと、follow-up の `thread-20` は許可されること、新形式の完全入力が維持されることを確認。関連 12 件すべて成功。
- **R2** — `ee858c0a` (`fix(runtime): authorize search cursor positions before use`): DB rowid の cursor 参照位置を caller の共通 visibility ゲートで再認可してから使用し、不存在・不可視・解決不能はすべて同じ `thread_cursor_invalid` にする。候補の系譜に含まれる不可視 fork 行は cursor 発行前に除外し、発行済み cursor が自身の認可チェックで拒否されるのを防ぐ。cursor は認可証拠にはしない。
  - 回帰テスト: 可視集合が空の caller と自プロジェクトを持つ caller から、他プロジェクトの実在 rowid と不存在 rowid が同一エラーとなること、可視時に使えた cursor も認可を失うと拒否されること、110 件の不可視 fork 候補を cursor として発行しないことを確認。`thread_read` の cursor は認可済み run 内の位置だけを検証し、`thread_grep` は数値 offset のみで DB/run の参照解決をしないことを確認し、実在する不可視／不存在の run ref と target の拒否を回帰テストに追加。thread 関連 29 件すべて成功（既存の 2,106 スレッドの検索継続も含む）。
- 最終検証（コード最終 `ee858c0a`、ADR 対応記録を含む対象 HEAD `3b4afbd2`）: 権限昇格、`CARGO_BUILD_JOBS=4`、`CARGO_TARGET_DIR=$PWD/target`（この worktree のみ）、`TMPDIR=/tmp/evt`（`mkdir -p` 済み）で実行。
  - `cargo nextest run -p storage -p runtime -p agents -p gui --locked -j 4`: 3,532 件すべて成功、42 件 skip（nextest run `1489a46b-152d-4620-8b23-8f8f15432f85`）。
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`: 成功、警告なし。
  - `cargo fmt --all --check`: 成功、差分なし。
  - `scripts/check-cache-contracts.sh -j 4`: 263 件すべて成功（`thread_tools_cache` 2 件を含む。nextest run `426d9fb3-3a4e-422f-9107-b2f1b4ef6198`）。
  - 今回の検証ではコード起因・環境起因の失敗ともになし。実 store は不使用。R1 / R2 と ADR 対応記録の 3 コミットは GitHub API で `verified=true, reason=valid` を確認済み。
- 両修正は署名付きコミット・通常の fast-forward push 済み。fmt / pre-push clippy フック成功。`crates/tools` と ADR の触らない範囲は変更していない。

### 触らない範囲

別の担当が変更中のため、次には触れない。`crates/tools` の edit / executor、runtime の `identical_calls.rs`、`budget_tracker.rs`、`completion_relay.rs`。

## 再開手順

1. worktree `/home/turtton/.ghr/github.com/turtton/evorch/.worktrees/thread-context-ref`（ブランチ `feat/thread-context-ref`、起点は origin/main の 86d5b332。2026-10-25 に merge コミット 091442b1 で origin/main の d3900116 を取り込んだ）に移動する。メインのチェックアウトは触らない。
2. この ADR を最初から読み、「実装状況」で最初の未チェック項目から再開する。
3. `git log --oneline -- intents/evorch/decisions/0041-thread-context-reference.md crates/storage crates/runtime/src/meta crates/gui/src/app` などで、チェック済みでもコミットが残っているか確かめる。
4. マイグレーション番号、`HistoryRead` の位置、`capabilities()` の形などは、実装前に現行のコードで確認する（この ADR の番号は 2026-10-24 時点）。
5. コミットは署名つきで、`type(scope): subject` 形式にする。マージは「実装状況」の PR の行の条件（CI 成功と独立レビュー合格）を満たしてから squash で行う。

## Phase B 実装状況チェックリスト

Phase B の進捗を記録する。各項目の完了時に `[x]` にして、commit SHA と検証結果を記入する。設計は「Phase B の設計」に従う。

- 作業場所: worktree `/home/turtton/.ghr/github.com/turtton/evorch/.worktrees/thread-conversation-search`、ブランチ `feat/thread-conversation-search`（起点は origin/main の `870a969b`）。メインのチェックアウトと他の worktree の `target` には触らない。
- コミットは署名つきで、`type(scope): subject` 形式にする。force push はしない。区切りごとに commit と push を行う。
- ユーザー許可（原文）:「推奨で、マージまでOK」。worktree で作業 → 計画の独立レビュー → 実装 → 別の Reviewer による受け入れ → push と PR → CI 成功で squash マージ（`--match-head-commit`）。

### 計画と契約

- [x] ADR に計画記載 — commit: この節を書いたコミット（`git log -- intents/evorch/decisions/0041-thread-context-reference.md` で確認）。Phase A の merge commit（`5d23cab4`）と Phase 0 計測の要約も同じコミットで記録した。
- [x] plan review 指摘 R1–R5 反映 — provenance の保存・変換 API、状態遷移表、物理 reserve / admission、normalize / guarded note、CLI request / owner の契約と受け入れケースを追加。S1 の単独ファイル owner と S2 の計測条件も反映。commit: 本項目を追加した `docs(adr): address Phase B plan review contracts`（SHA は `git log` で確認）。コード変更なし。差分範囲・B-1〜B-4 不変・必須契約の記載を確認（実装テストは未実施）。
- [ ] 計画の独立レビュー（plan review）— M1〜M6 と R1〜R5 / S1〜S2 を反映済み。改訂後の計画に対する独立 Reviewer の合格判定は未取得。判定:
- [ ] 順序 1 契約の固定 — R1 の保存形式・全変換・W1/W2/W4 API / fixture、R2–R5、設定型・API とファイル所有表を固定。順序 2 / 3 の開始ゲート。storage 共有契約は `crates/storage/src/thread_index.rs` / `thread_index/contracts.rs` に型・trait として定義（SQL / writer / job の実装ではない）。`read.rs` は W1 単独所有。**W3 の strict 設定型・検証の合流が全体の残ゲート**（ユーザー確認済み。本担当は設定取り込み API のシグネチャまでで W3 の実装に触れない）。commit: 本項目更新コミット（`git log` で確認）
  - [x] storage 共有型・API・所有表 — `6f632712`（`feat(storage): define Phase B conversation index contracts`、GitHub Verified）。抽出/commit 補助契約と fixture は後続の最小抽出器コミットに含む。
  - [ ] W3 strict 設定型・検証の合流（順序 1 全体の残ゲート）
- [ ] 順序 2 最小の抽出器で計測 — W1 の抽出 / 会計関数を W6 から呼ぶ。provenance 不明を native と仮定しない。commit:
  - [x] storage の最小抽出器・境界/除外/provenance fixture と runtime normalize の共通比較契約テスト — commit: 本項目追加コミット（`git log` で確認）。計測ハーネス・実 store 計測・共通会計実装/係数は未実施のため順序 2 全体は未完了。
- [ ] 順序 3 migration（v25）・抽出・状態遷移 — provenance 原子的保存・復元を含む。commit:
- [ ] 順序 4 不可分な置換（索引の置換・evict・source 削除の tombstone）— commit:
- [ ] 順序 5 開始ゲート — reserve の R_floor / safety_factor / F、G の上界係数、再開 J、step 上限、前景待ちの絶対増分・倍率を実測から ADR に数値で固定。結果:
- [ ] 順序 5 job・lease・上限 — commit:
- [ ] 順序 6 検索・cursor・ref — commit:
- [ ] 順序 7 CLI（`reindex-threads`）・設定・spec — commit:
- [ ] 順序 8 実 store のコピーで検証 — commit:
- [ ] 順序 9 ADR の仕上げ — commit:

### 検証・レビュー・公開

- [ ] 検証（`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo nextest run`、`scripts/check-cache-contracts.sh`）— 対象 HEAD と結果:
  - 契約・最小抽出器の担当範囲（`6f632712` + 最小抽出器コミットの変更内容）: `CARGO_BUILD_JOBS=4 cargo nextest run -p storage -p runtime --locked -j 4` は **2,115 件成功、8 件 skip**（抽出器 14 件、normalize 共通比較 2 件、型の契約 3 件を含む）。`cargo test -p storage -p runtime --doc --locked -j 4` は storage 8 件成功、runtime 0 件。`cargo clippy --workspace --all-targets --locked -- -D warnings` は警告なしで成功、`cargo fmt --all --check` / `git diff --check` も成功。
  - すべて指定 worktree の target・権限昇格・並列度 4 で実施。SUN_LEN 等の環境起因失敗なし。実装中の test assertion（`context_len` と本文キーの誤判定）・helper の borrow 型注釈・provider image fixture の `source` 不足は修正後に再検証済み。固定 sleep の追加なし。
  - `scripts/check-cache-contracts.sh` はこの区切りでは未実施（runtime の production normalize / provider 入力は変更せず、テスト module の追加だけ）。Phase B 全体の cache 契約検証・受け入れ・検索/CLI/FTS は後続のため本項目全体は未完了。
- [ ] FTS の `integrity-check`（rank=1）が受け入れテストで成功する — 結果:
- [ ] R1: fork → compaction → 再起動 / 保存途中障害で native な別発言を誤って除外しない。snapshot / sidecar / revision の原子性と未知理由 — 結果:
  - 最小抽出器の synthetic fixture で copy/native 同文の別位置・生成境界・serialized sidecar 再読込・不明理由の保留・snapshot/sidecar 混在拒否を確認済み。実際の生成経路・SQLite 保存障害/rollback/再起動の統合検証は未実施。
- [ ] R2: evict → 上限引き上げ → snapshot/note 更新 → 再起動で復活しない。blocked の試行数保持・全遷移・tombstone / 遅延 commit — 結果:
- [ ] R3: 論理空きあり / 物理 reserve 不足で索引停止、正本書き込み余裕維持、2 サイクル失敗 / WAL reader / 再開境界 — 結果:
- [ ] R4: 画像+text、2048 B 超 error、明示 tool_result、note 更新・clear 後の guarded reference — 結果:
  - 抽出と normalize 比較では画像+text・画像 note と同文の本物の text・2048 B 超 error の全文保持を確認済み。note の最新入力・空 clear の block なしと型上の locator 分離も確認。検索の kinds 和集合・guarded ref の正本解決/認可/stale は未実装。
- [ ] R5: owner × wait の動作表、対象 revision 固定、各 gap / failed、owner 異常終了・lease 引き継ぎ — 結果:
- [ ] 受け入れレビュー（実装者とは別の独立した Reviewer）— 対象 HEAD と判定:
- [ ] PR 作成 — PR:、head:
- [ ] CI 成功（PR の最新 head SHA に対する必須 check）— head と結果:
- [ ] マージ（squash、`--match-head-commit`）— merge commit:
- [ ] 後片付け（マージ確認後の worktree とブランチの削除）— 結果:

### 計測

計測は実 store のコピー（`mode=ro` で開き backup API でコピー。原本には書き込まない）で行う。

- [ ] 切り詰め後の本文（kind 別）、FTS、補助テーブル、WAL のピーク、回収後のサイズ — 結果:
- [ ] `max_bytes` ごとの保持範囲（決定 B-2）— 結果:

  | `max_bytes` | 保持できる run（N / 全体） | 索引化可能 run 数 | 対象 / 保持本文量（B） | 最古の時刻 | 保留（pending）の数 | oversized の数 |
  |---|---|---|---|---|---|---|
  | 256 MiB | | | | | | |
  | 512 MiB | | | | | | |
  | 1 GiB | | | | | | |

  512 MiB が明らかに不適切なら、マージ前に報告する。
- [ ] `provenance_unknown` の数（reason_code 別、全体と索引化可能母集団の差も報告）— 結果:
- [ ] 同じ抽出器 / 会計 version・係数、短語・regex 含む確実に hit がある dataset / 正解集合 — 結果:
- [ ] 検索 p95 < 300 ms（release ビルド、warm / cold、backfill あり / なし、語の種類別）— 結果:
- [ ] 前景の書き込み待ち（索引無効との p50 / p95 / 最大・絶対増分 / 倍率比較、許容値と超過時の再計測・判断）— 結果:
- [ ] DB+WAL の物理増加上界、reserve・停止/再開閾値と正本余裕の実測 — 結果:
- [ ] 「実装時に確認すること」の全項目の確認結果を記録した — 結果:

### Phase B の再開手順

1. 上の worktree に移動する。メインのチェックアウトには触らない。
2. 「Phase B の設計」と、このチェックリストの最初の未チェック項目から再開する。
3. マイグレーション番号（v25）、`writer/state.rs` の行番号などは、実装前に現行のコードで確認する（この ADR の番号は `870a969b` の時点）。
4. 実 store を使う作業は、原本を `mode=ro` で開いて backup API でコピーしたものだけに行う。
