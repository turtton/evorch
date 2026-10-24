# ADR 0041: 他スレッドの会話を参照する（thread_search / thread_grep / thread_read）

## Status

Accepted（2026-10-24）。利用者との相談で、以下の設計判断（a〜i と権限モデル、Phase A の設計）をすべて推奨案どおりに承認した。Phase A の実装は一部着手済みで、進捗は末尾の「実装状況」に記録する。

この ADR は、evorch の再起動で作業が失われても、次の担当がこの ADR だけを読んで再開できるように、承認済みの仕様をすべて書く。

- 着手順は Phase 0（ADR と計測）→ Phase A（storage / runtime / GUI）である。Phase B（本文の全文索引）は Phase 0 の計測を見て別途決める。Phase C（ポインタ注入・GUI 添付）は今回の範囲外である。
- Phase 0 の計測結果は、以下の「Phase 0 計測」節に記録する。計測は完了している。Phase B の採否は別途判断する。

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

- 本文検索（Phase B）は FTS5 の trigram を使う。2 文字以下の語は trigram で引けないため、`LIKE` で代用する。
- Phase A では FTS を使わない。
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
| Phase B | 本文の全文索引（FTS5 trigram）。Phase 0 の計測を見て別途決める |
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

Phase 0 の計測は完了した。正規化時間は Python の JSON parse・テキスト化による参考値であり、Rust 実装性能を示すものではない。FTS の全量サイズは 10% サンプルの SQLite DB ファイル実測値を単純に 10 倍した外挿であり、SQLite の管理 overhead を含むサンプル値から全量索引の実サイズを保証するものではない。

## 実装状況

各項目の完了時に `[x]` にして、commit SHA を記入する。

- **origin/main への rebase は不要と判断した（origin/main 側の差分は crates/gui の #335 のみで、v24 と衝突しない）。PR 作成前に `git merge-tree` で衝突がないことを再確認する。**
- [x] ADR 作成 — commit: （この ADR を追加したコミット。`git log -- intents/evorch/decisions/0041-thread-context-reference.md` で確認）
- [x] Phase 0 計測 — commit: 計測結果を記入したコミット（`git log -- intents/evorch/decisions/0041-thread-context-reference.md` で確認）
- [x] A-1 storage — commit: 35021ada
- [ ] A-2 runtime — commit: ____
- [ ] A-3 GUI — commit: ____
- [ ] A-4 inspect（任意）— commit: ____
- [ ] 検証（`cargo nextest run`、`cargo clippy --workspace -- -D warnings`、`cargo fmt --check`、`scripts/check-cache-contracts.sh`）— commit: ____
- [ ] 受け入れレビュー — commit: ____
- [ ] PR 作成（マージはしない）— PR: ____

### 触らない範囲

別の担当が変更中のため、次には触れない。`crates/tools` の edit / executor、runtime の `identical_calls.rs`、`budget_tracker.rs`、`completion_relay.rs`。

## 再開手順

1. worktree `/home/turtton/.ghr/github.com/turtton/evorch/.worktrees/thread-context-ref`（ブランチ `feat/thread-context-ref`、起点は origin/main の 86d5b332）に移動する。メインのチェックアウトは触らない。
2. この ADR を最初から読み、「実装状況」で最初の未チェック項目から再開する。
3. `git log --oneline -- intents/evorch/decisions/0041-thread-context-reference.md crates/storage crates/runtime/src/meta crates/gui/src/app` などで、チェック済みでもコミットが残っているか確かめる。
4. マイグレーション番号、`HistoryRead` の位置、`capabilities()` の形などは、実装前に現行のコードで確認する（この ADR の番号は 2026-10-24 時点）。
5. コミットは署名つきで、`type(scope): subject` 形式にする。マージはしない。
