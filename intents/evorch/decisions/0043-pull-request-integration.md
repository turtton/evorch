# ADR 0043: スレッドに紐づいた PR の状態をハーネスが取得し、表示・通知・審査に使う

## Status

Accepted（2026-10-28）。利用者との相談で、下の「Decision」の方針（GUI は案 A）に合意した。2026-11-05 に Phase 1 の実装を完了（#481: storage と設定、#485: gh source と checkout 照合、#489: runtime の追跡サービス・ownership・候補・継承・event、#492: 審査の harness_facts と manifest、#493: GUI、#495: 統合 e2e）。2026-11-07 に Phase 2 の実装を完了（#509: PR バーのタイトル表示、#507: CI ログ source、#510: storage と outbox の一般化、#518: harness notice の配送、#517: coordinator・auto の適格判定・帰属、#519: GUI の操作・harness カード・上限の通知）。2026-11-10 に Phase 2 の残課題に対応した（#526、#528、#530、#532、#538）。

## Context

PR を使うスレッドでは、CI の結果、head SHA、マージ状態、レビューコメントがハーネスの外にある。agent は自分で `gh` を実行しない限りそれを知らず、ハーネスと利用者も、スレッドの PR が今どうなっているかを一か所で見られない。

- CI が失敗しても、agent は人が伝えるまで気づかない。
- escalation の自動審査（[ADR 0033](0033-file-tool-path-boundary.md)）は、審査役が PR・CI・git の状態を見られない。auto review の拒否を分類したところ、約 10% が「状態が見えない」ことによる拒否だった。承認自体は認めつつ「状態が確認できない」として拒否した例が 33 件あった（利用者提示の分類。集計の再現手順はこの ADR に含めない）。たとえば「PR がマージ済みで head が一致していること」が前提の `gh pr merge` 後の後片付けでは、前提を審査役が確かめられない。
- 一方で、PR を使わないスレッドもある。PR の存在を前提にした指示や取得を全スレッドに持ち込むと、不要な負荷と誤った審査入力になる。

既存の部品は次のとおり。

- 内部通知: `AgentRuntime::deliver_notice`（[ADR 0036](0036-thread-replay-evaluation.md) の評価完了通知が使う）。実行中・待機中の run に、ユーザー権限なしで届く。
- 終了済み run への配送: 保留メッセージの仕組み（`held_messages`）。受信側が再登録されるまで保留し、wake 回数は `AUTO_WAKE_LIMIT` で会話ごとに制限する。operator が止めた run は起こさない。
- 審査入力: `ReviewContext` と `EvidenceStamp`（`crates/runtime/src/escalation_review.rs`、`escalation_review/gate.rs`）。`ReviewContext` には実ユーザーの依頼、委譲経路、承認済みのプロジェクトルールが入る。
- 外部入力の扱い: SecretGuard と、未信頼テキストを指示と区別する囲み。
- キャッシュ契約（[ADR 0003](0003-cache-first-context-engine.md)、`AGENTS.md`）: 通常ターンでは送信済みの system・tools・入力 prefix を変えない。

## Decision

### 目的と非目的

スレッドに PR が紐づいたときだけ、ハーネス本体が PR の状態（CI check、head SHA、マージ状態、レビューコメント）を取得して追跡し、次の 3 つに使う。

1. GUI に表示する。
2. CI の失敗を agent に通知して修正を促す。
3. 同じ事実を escalation review に「ハーネスが取得した事実」として渡す。

**PR を使わないスレッドには何も持ち込まない。** 紐づけがなければ、取得も通知も審査入力の項目もなく、審査の指示文も PR を前提にしない。

### 取得は gh CLI 経由

- ハーネス本体が sandbox の外で `gh pr view --json …`、`gh api`、`gh run view --log-failed` を実行する。evorch はトークンを保持しない。
- `PullRequestSource` trait で取得を差し替え可能にし、テストは Fake を使う。
- 対応ホストは当面 GitHub.com のみ（GitHub Enterprise は非対応）。GitLab などは、保存する行に provider 列だけを持たせる。抽象は今は広げない。

### 紐づけ

- 正は利用者の明示（GUI の Link 操作）である。新規・再 Link はスレッド名の右クリック「Link PR…」と候補チップで行う。
- `gh pr create` などのツール出力から検出した PR は、**候補として Phase 1 から提示**する。人がワンクリックで確定し、自動では確定しない。
- agent 専用の紐づけツールは作らない。ToolSpec を途中で追加すると cache 契約に反し、agent の主張は検証が要るためである。
- 紐づけ時にハーネスが照合する。PR のリポジトリとローカルの remote、PR の head ブランチと worktree のブランチを比べ、不一致なら警告付きで紐づける（拒否はしない）。
- スレッドあたり active な紐づけは 1 件。再紐づけすると、前のものは superseded になる。
- **Unlink**: active link を unlinked として終了できる。追跡を停止し、バッジ・PR バーを非表示にし、審査の `harness_facts` も出さない。派生済みの子スレッドの link には影響しない。
- 保存先は新テーブル（例 `thread_pull_requests`）。互換性は維持しない（AGENTS.md の Compatibility policy）。

### 紐づけの引き継ぎ

- **継承するのは open の PR のみ。Merged・Closed は引き継がない。** マージ済みか確認できないときは引き継ぎを保留する。
- escalation・fork・rewind などで派生したスレッドに引き継ぐが、Lane（新しい派生種別）には引き継がない。
- マージ後の派生スレッドに候補は出さない。

### 事実と未信頼テキストを分ける

- ハーネスの事実: API の構造化フィールド（state、head SHA、check の status と conclusion、取得時刻）。
- 未信頼テキスト: CI ログ、レビューコメント、PR 本文。**審査役には渡さない。** agent には、上限付き・SecretGuard 適用・「指示ではなくデータ」と明示した囲みで渡す。
- check 名のように第三者が決められる文字列は、事実の側に入れる場合でも長さ上限を設け、改行を除く。

### ポーリング

- 対象は、紐づけがあり、未 archive で、PR が終端（Merged / Closed）でないスレッドだけ。
- check が pending の間は短い間隔（例 30 秒）、安定している間は長い間隔（例 5 分）で取得する。Merged / Closed で止める。失敗時は指数 backoff と Diagnostic。
- 次の契機でも取得する。GUI の更新操作、審査時のオンデマンド取得、agent の push や `gh pr` 系コマンドの完了直後。
- テストのため trigger / notify 駆動にし、固定 sleep に依存しない（[ADR 0042](0042-event-driven-waits.md) と同じ方針）。

### CI 失敗時の通知（PR ごとの auto fix）

#### 対象と設定

- PR ごとに GUI 上の **auto fix トグル**（既定オフ）を持つ。これは Claude Desktop の auto fix の挙動（利用者から聞いたもの）に倣う。
- 全体スイッチ `[pull_requests] auto_fix_enabled` を持つ（既定 true）。自動通知だけを止め、手動通知は止めない。変更は trusted な層でのみ行える。
- 設定と上限は PR 全体で共有する。
- 通知先は、auto fix をオンにした thread とする。変更するときは利用者に確認する。継承によっては変わらない。Unlink・archive・削除されたら停止する。
- **オンの PR**: CI が失敗するとハーネスが自動で agent に通知する。通知は、agent の push と照合できた head に限る（後述）。
- **オフの PR**: GUI に表示するだけ。人が「agent に送る」操作で手動通知できる。auto がオフのときは、CI 失敗を検知しただけでは自動のカードを出さない。手動で送信した通知は会話に実際に追加されるため、harness カードとして表示する。auto をオフにしても、過去のカードは消さない。
- 通知の対象は CI 失敗だけとし、レビューコメントは含めない。
- 他の check が pending の間、auto は待つ。手動は即時に送れる。このため、GUI が Failing と表示していても、auto fix がまだ動いていない期間がある。この差は仕様である。

#### 上限と停止

- 同じ (repo, PR, head SHA) への通知は、手動と自動を合わせて 1 回まで。
- PR ごとの通知は 3 回まで、cooldown は 10 分。回数は採用時に数え、予約も含める。
- 上限に達したら auto fix を自動でオフにする。再開は明示的なリセットだけで行い、SHA の履歴は消さない。
- wake の歯止めは、ADR 0044 の「発生元ごとの、進展のない wake 20 回」を使う。held 側の `AUTO_WAKE_LIMIT` は使わない。source key は PR 単位とする。
  - guard がリセットされるのは、実ユーザーの入力か、明示的な Resume があったときだけ。head の更新、ACK、poll ではリセットしない。
- operator が Stop した run は、自動で起こさない。Stop は durable Stop を使う。Stop のあとに手動で送るには、明示的な Resume が必要。

#### push 照合

- 自動通知するのは、agent の push と照合できた head だけ。次の両方を満たすとき、その head を agent 由来とみなす。
  1. 前回採用した head 以降に、同じ thread で publishing の成功完了を観測している。同期 shell では exit_code == 0 を優先して判定する。
  2. 採用時の local_head が、新しい PR head と一致する。
- Link 時の緩和: Link する時点で、同じ thread の publishing の成功がすでに観測されていて、かつ local HEAD が PR の head と一致していれば、その head を agent 由来とみなす。Link 前の push（`gh pr create` の直後に候補から Link する流れなど）で、最初の CI 失敗が自動通知されないことを避けるためである。
- 上記に当てはまらない head は unknown とし、自動通知しない。unknown の head も GUI の表示と手動送信は使える。
- 再起動後で、まだ予約していない head は unknown として扱う。
- これは actor の証明ではなく、安全側に倒した推定である。

#### 配送

- Running、Waiting、Done のいずれの run にも、ADR 0044 の durable lane outbox を harness notice 用に一般化したもので配送する。held → wake を拡張する案と、PR 用に `deliver_notice` を流用する案は採らない。
- 発生源の種別（Lane / PullRequestChecksFailed）を区別する。PR を偽の lane として扱わない。
- claim は 1 種類の source だけで構成する。
- notice に安定した ID を持たせる。
- flush の境界で、snapshot・receipt・PR のカウンタ・guard を 1 つの transaction で採用する。
- Done の run は、host 側で restore してから起こす。復元できない場合は typed NACK を返す。
- receipt の provenance は HarnessNotice とし、実ユーザーの証跡には入れない。通知の本文が `real_user_requests` になってはならない。
- 通知の表記は `[harness-notice kind=pr_checks_failed pr=#… head=…]` に、事実と、未信頼の CI ログ抜粋の囲みを続ける。通知は権限を与えない。
- キャッシュ契約: 通知は会話の末尾に追記するだけにする。

#### CI ログと表示用タイトル

- CI ログは最大 3 run まで取得する。生出力は 1 MiB まで、抜粋は 8 KiB / 200 行まで、通知全体は 16 KiB まで。取得は 20 秒で打ち切る。
- SecretGuard は切り詰める前に適用する。
- `<ci-log-data>` の内側はエスケープする。
- provider_id はそのまま使わない。run と head の対応を確かめたものだけを使う。
- PR のタイトルは GUI 専用の display 型に入れる。facts や審査には含めない。SecretGuard を適用し、単一行にして上限をかける。
- UI 上で不要な注意書きは出さない（「外部データ — 信頼しないこと」、「ユーザー発言ではない」など）。

### 審査への供給

- `ReviewContext` に `harness_facts` を追加する（例: `pull_request_state`、`local_worktree_head`）。紐づけがなければ項目自体を出さない。
- 審査時は、取得から一定時間内なら追跡結果を使う。古ければ短いタイムアウトで再取得し、失敗したら stale の印を付けて渡す。
- facts の更新で `EvidenceStamp` を変えない。stamp は実ユーザー証跡の同一性だけを表す。
- 指示文は一般的な語彙で 1 文にする。含める内容は次のとおり。
  - 事実は承認ではなく、状態の証拠である。
  - 状態の前提を確かめるのに使う。
  - 事実がないことは、条件が満たされないことを意味しない。
  - stale な事実は弱い証拠である。
- 将来、手順の前提（例: 「merged かつ head が一致」）をコードで判定する余地を残す（[conversation-procedures](../../../docs/conversation-procedures.md) の procedure_step）。今回は範囲外。
- 審査入力の manifest（予定）には、`harness_facts` のハッシュを含める。

### ローカル worktree の HEAD

- `local_worktree_head` は、ハーネスが git で取得してよい。
- ただし、許可ディレクトリ（[ADR 0032](0032-project-allowed-directories.md)）や worktree が複数あるとき、PR が root project path 内のリポジトリとは限らない。
- そこで、PR の head リポジトリとブランチを、スレッドが触れうる各ディレクトリ（project root、worktree、許可ディレクトリ）の remote とブランチと照合し、対応するチェックアウトを特定する。
- **一意に決まらなければ `local_worktree_head` は出さない。** 推測しない。
- 照合方法の詳細は未決。

### 設定

- `[pull_requests] enabled = false`（opt-in）。
- ほかに、ポーリング間隔、ログ抜粋の上限、審査への供給の有無を設定にする。名前と既定値は実装時に決める。
- auto fix は PR ごとのトグルとする。自動通知だけを止める全体スイッチ `[pull_requests] auto_fix_enabled`（既定 true、trusted な層でのみ変更可）を持つ。

### 段階

- **Phase 1**: 紐づけ（明示と候補の提示）、取得、GUI 表示、審査への事実の供給。
- **Phase 2**: CI 失敗の通知。手動送信と、PR ごとの auto fix による自動通知（終了済み run の wake を含む）。旧案の Phase 3（全体設定による自動再開）は Phase 2 に統合した。PR 構成は次のとおり。
  - **T**: タイトルの取得と GUI 専用の display 型。
  - **A**: storage と outbox の一般化（ADR 0044 の durable outbox を harness notice 用に一般化し、PR 単位の guard と receipt を含める）。
  - **B**: ログの source（CI ログの取得、上限、SecretGuard、エスケープ、run と head の対応）。**C**: 配送（Running・Waiting・Done、host の restore、typed NACK、durable Stop）。B と C は並行して進める（B ∥ C）。
  - **D**: coordinator（push 照合、auto fix の判定、手動送信、回数上限と cooldown、全体スイッチ、Link 時の緩和）。
  - **E**: GUI（PR バー、CI 失敗時の展開、auto fix トグル、harness カード、Notifications）。
  - 順序は T → A → (B ∥ C) → D → E とする。

### GUI

最新の情報と操作は画面下部に置く。mock で 3 案を比べ、案 A を採る。

- **PR バー**: 会話の入力欄（composer）の直上に置く。折りたたみ時は 1 行で、展開すると上方向に伸びる。上限は会話欄の 40% とし、狭い画面は想定しない。CI 失敗時は展開する。check の一覧、head SHA、マージ状態を表示し、操作行には更新と「Unlink」を置く（「Link…」は置かず、新規・再 Link はスレッド名の右クリック「Link PR…」と候補チップで行う）。「agent に送る」操作と auto fix トグルは Phase 2 の機能のため Phase 1 では表示しない（Phase 2 では CI 失敗時に展開し、auto fix 動作中は畳む）。PR バーには番号とタイトルを表示し、branch は詳細に回す。
  - PR はスレッド単位なので、subagent の transcript を開いている間も PR バーを表示する。
- **スレッド一覧**: 行に PR バッジ（#番号と ✓ / ✗ / ● / merged）を出す。
- **候補 PR**: `gh pr create` の出力の直下にチップ（Link PR / 無視）を出す。手動の「Link PR…」は、スレッド名の右クリックメニューに加える。
- **ハーネスの通知**: transcript では `harness` タグ付きのカードにし、ユーザーの発言や agent のメッセージと区別する。UI 上で不要な注意書き（「外部データ — 信頼しないこと」、「ユーザー発言ではない」など）は出さない。未信頼テキストの区別は agent / 審査への入力側（囲み・型）で保つ。
- **Notifications**: CI failed、Merged、auto fix の上限到達を加える。
- **テーマ**: merged 用の紫は Graphite `#a371f7`、Tokyo Night `#bb9af7` に決定。
- 細部（ピクセル等）は実装時に決める。

採らなかった案:

- 案 B（専用の PR パネル）: 会話から視線が離れ、Subagents と同時に見えない。必要になれば、PR バーを補う詳細パネルとして将来加える余地はある。
- 案 C（Diff タブへの統合）: 既定の左カラム幅に収まらず、Notifications と同じタブ群に入るため隠れる。

### 実装で確定した仕様

Phase 1 の実装を通じて次の仕様を確定した。

- 事実の採用は success_seq の新旧で判定する。
- 取得を行うのは thread の owner プロセスだけ。owner でない側は表示のみ。GUI の操作は chat_permit を経由する。
- gh が check なし（null）を返した場合は checks_complete=false として扱い、成功とは扱わない。
- 障害は FetchStatusChanged で伝え、PR 用の Diagnostic は出さない。
- レビュー情報は reviewDecision のみ（本文と件数は扱わない）。
- 審査の anchor は lifecycle_revision。
- web/upload の審査は snapshot だけを使う。
- PR バーには head branch を表示していた（タイトルは取得していなかった）。Phase 2（T）でタイトルを取得し、PR バーは番号とタイトルを表示する。branch は詳細に回す。

### Phase 2 の実装で確定した仕様

Phase 2 の実装を通じて次の仕様を確定した。

- 帰属判定に使う成功シグナルは、`git push` と `gh pr create` の成功完了だけとする。read-only の gh や dry-run（省略形を含む）は対象外。Phase 1 の refresh 契機とは別の分類にする。
- 予約に対する ready/cancel は、reservation_revision の CAS で ABA を防ぐ（migration v52）。
- GUI の「External commit」表示は、coordinator の attribution（Unknown）に基づいて出す。local HEAD との一致だけでは agent 由来とみなさない。
- 全体スイッチがオフのときは、GUI に「Off by settings」と表示する。手動送信は可能。
- 上限到達の通知は at-most-once とする。claim が成功してから GUI が受け取るまでの間に異常終了すると通知が欠けるが、重複を防ぐことを優先して許容する。
- owner の判定は、worker が claim を採用した時点で行う。その後、キューで待っている間に所有権が移った場合は、旧 owner の window に 1 件出ることを許容する。

## Consequences

- 紐づけがあるスレッドの審査は、PR・CI・ローカル HEAD の状態を根拠にできる。その分、拒否の一部（状態が見えないことによる拒否）が減ると期待する。実際の減少は未計測。
- `gh` の認証状態に依存する。未認証や失敗は Diagnostic にし、事実は stale または不在として扱う。
- ポーリングの取得頻度は GitHub API の rate limit に影響する。間隔と backoff で抑える。
- 新テーブルと、runtime の定期取得、審査入力の項目、GUI の画面が増える。
- 自動通知は agent の実行を増やす。歯止め（SHA 単位の 1 回、PR ごとの 3 回上限と 10 分の cooldown、発生元ごとの wake guard 20 回）、PR ごとの既定オフ、全体スイッチで抑える。

## Alternatives

- **審査時だけ都度取得し、追跡しない最小案**: 実装は小さいが、GUI 表示と CI 失敗の通知に使えず、審査のたびに外部呼び出しの待ちが入る。
- **agent に `gh` を実行させ、自己申告させる**: agent の主張は検証できず、審査役に渡せる事実にならない。
- **Webhook / GitHub App**: ローカル実行の evorch に公開エンドポイントと App の導入を要求する。トークン・秘密の管理も増える。
- **agent 専用ツール `pr_link` / `pr_status`**: ToolSpec の途中追加が cache 契約に反する。紐づけは agent の主張になり、検証が要る。
- **GitHub API を直接呼ぶ**: トークンを evorch が保持する必要がある。`gh` の認証に任せる。

## 未決事項

- 複数ディレクトリがあるときの、PR とチェックアウトの照合方法の詳細。

## 既知の制限・残課題

- 別の thread を別の owner が持っていて、それぞれが同じ PR を追跡している場合、取得が重複する。
- gh の孫プロセスは kill されない。
- ssh.github.com などの remote 別名は照合しない。`branch.<name>.remote` で fork を参照している場合も照合しない。
- checks のページ取得中に head が変わった場合の契約テストがない。
- pending 継承の一覧に cursor がない（先頭 1000 件のまま）。
- 同じ seq を採用した場合に、1 回だけ前倒しで取得する。
- 実際の routing fallback と、実際の CAS 拒否を通す統合テストがない。
- live run への誤配送を検出するテストがない。
- 主モデルと fallback モデルでの審査効果の評価は、まだ行っていない。
- `pull_request_review_cache` の CI で、まれに失敗することがある。`an_upload_review_reads_the_stored_facts_and_starts_nothing` が #538 の CI で 1 回だけ失敗し、再実行で通った。原因は未調査。以前記録された 180 秒停止についても、CI 上の記録は特定できていない。

## 外部の参考

- Copilot CLI `/pr fix ci`: 利用者の操作が起点。
- Cursor Cloud Agents: 自分で作った PR の Actions 失敗に条件付きで対応する。人の push や base でも落ちる check には追従しない。
- Conductor: Checks ビュー。
- Devin: CI Error Resolution のテンプレート。
- Orca（stablyai/orca）: PR を worktree に紐づけて扱う。CI 失敗から agent を起動する案は issue #10131 にあり、opt-in・cooldown・絞り込み付きの提案段階。
- t3code: CI の監視から agent を再開する issue のみ。
- Claude Desktop の auto fix: 利用者から聞いた挙動として記録する。公開資料での確認は未了。

## Validation and limits

- 各 PR の cache 契約テストを `scripts/contract-test-filters.sh` の `CACHE_CONTRACT_FILTER` に登録し、回帰テストで検証した。Phase 2 では、通知が末尾追記のみであることを `pull_request_auto_fix_cache` と `harness_notice_delivery_cache` で検証し、同じフィルタに登録した。
- 審査の指示文と `harness_facts` が、実際のモデルの判断（拒否の減少、誤承認の増加がないこと）に与える影響は未評価。主モデルと fallback モデルで比較する。
- 「約 10%」「33 件」は利用者から示された分類である。ADR 作成時に store で再集計していない。

## Related

- [ADR 0003](0003-cache-first-context-engine.md): 末尾追記のみの cache 契約。
- [ADR 0011](0011-release-and-diagnostics-ops.md): GitHub Issue 化では送信前に利用者確認を必須にした。外部サービスへの書き込みは行わない本 ADR とは別だが、`gh` を使う運用の前例として参照する。
- [ADR 0022](0022-parent-child-tree-addressing-and-nested-delegation.md): 親子・派生スレッドの扱い（紐づけの引き継ぎの前提）。
- [ADR 0032](0032-project-allowed-directories.md)、[ADR 0033](0033-file-tool-path-boundary.md): 許可ディレクトリと escalation 審査。
- [ADR 0035](0035-advisor-consultation-boundary.md)、[ADR 0040](0040-advisor-stuck-point-triggers.md): 審査役（Reviewer）と相談役（Advisor）の境界。事実は承認ではない。
- [ADR 0036](0036-thread-replay-evaluation.md): `deliver_notice` による内部通知の前例。
- [ADR 0041](0041-thread-context-reference.md): スレッド単位の情報を run の名前規約から辿る制約（紐づけの保存先の判断に関係する）。
- [ADR 0042](0042-event-driven-waits.md): 固定 sleep ではなく出来事で動かす方針。
- [ADR 0044](0044-lane-threads.md): durable lane outbox と発生元ごとの wake guard。CI 失敗通知の配送と抑止の基盤。
