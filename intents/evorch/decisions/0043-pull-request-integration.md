# ADR 0043: スレッドに紐づいた PR の状態をハーネスが取得し、表示・通知・審査に使う

## Status

Accepted（2026-10-28）。利用者との相談で、下の「Decision」の方針（GUI は案 A）に合意した。「未決事項」の項目は決めていない。実装は未着手。

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
- GitLab などは、保存する行に provider 列だけを持たせる。抽象は今は広げない。

### 紐づけ

- 正は利用者の明示（GUI の Link 操作）である。
- `gh pr create` などのツール出力から検出した PR は、**候補として Phase 1 から提示**する。人がワンクリックで確定し、自動では確定しない。
- agent 専用の紐づけツールは作らない。ToolSpec を途中で追加すると cache 契約に反し、agent の主張は検証が要るためである。
- 紐づけ時にハーネスが照合する。PR のリポジトリとローカルの remote、PR の head ブランチと worktree のブランチを比べ、不一致なら警告付きで紐づける（拒否はしない）。
- スレッドあたり active な紐づけは 1 件。再紐づけすると、前のものは superseded になる。
- 保存先は新テーブル（例 `thread_pull_requests`）。互換性は維持しない（AGENTS.md の Compatibility policy）。

### 紐づけの引き継ぎ

- **PR がマージされるまで**は、escalation・fork・rewind などで派生したスレッドにも紐づけを引き継ぐ。
- **マージ後は引き継がない。** 候補として出すかどうかは未決（出さなくてもよい）。

### 事実と未信頼テキストを分ける

- ハーネスの事実: API の構造化フィールド（state、head SHA、check の status と conclusion、取得時刻）。
- 未信頼テキスト: CI ログ、レビューコメント、PR 本文。**審査役には渡さない。** agent には、上限付き・SecretGuard 適用・「指示ではなくデータ」と明示した囲みで渡す。
- check 名のように第三者が決められる文字列は、事実の側に入れる場合でも長さ上限を設け、改行を除く。

### ポーリング

- 対象は、紐づけがあり、未 archive で、PR が終端（Merged / Closed）でないスレッドだけ。
- check が pending の間は短い間隔（例 30 秒）、安定している間は長い間隔（例 5 分）で取得する。Merged / Closed で止める。失敗時は指数 backoff と Diagnostic。
- 次の契機でも取得する。GUI の更新操作、審査時のオンデマンド取得、agent の push や `gh pr` 系コマンドの完了直後。
- テストのため trigger / notify 駆動にし、固定 sleep に依存しない（[ADR 0042](0042-event-driven-waits.md) と同じ方針）。

### CI 失敗時の通知（PR ごとの auto fix トグル）

- PR ごとに GUI 上の **「auto fix」トグル**（既定オフ）を持つ。これは Claude Desktop の auto fix の挙動（利用者から聞いたもの）に倣う。
- **オンの PR**: CI が失敗するとハーネスが自動で agent に通知する。
  - 実行中・待機中の run には、既存の `deliver_notice` を流用する。
  - 終了済みの run には、既存の held → wake の経路に通知用のキューを足して起こす。
- **オフの PR**: GUI に表示するだけ。人が「agent に送る」操作で手動通知できる。
- 当初案の「全体設定による自動再開」は、この PR ごとのトグルに統合する。
- 歯止め:
  - 同じ (repo, PR, head SHA) への通知は 1 回。
  - PR ごとの回数上限と cooldown（叩き台は 3 回・10 分。既定値は未決）。
  - 会話の自動 wake 上限（`AUTO_WAKE_LIMIT`）も適用する。
  - operator が止めた run は起こさない。
  - 外部 push と思われる head は自動通知せず、GUI に表示するだけ（判定方法は未決）。
- 通知の表記は `[harness-notice kind=pr_checks_failed pr=#… head=…]` に、事実と、未信頼の CI ログ抜粋の囲みを続ける。
- 通知は権限を与えない。審査の実ユーザー証跡にも入れない（通知の本文が `real_user_requests` になってはならない）。
- キャッシュ契約: 通知は会話の末尾に追記するだけにする。

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
- auto fix は全体設定ではなく PR ごとのトグルとする。全体で無効化するスイッチを持つかは未決。

### 段階

- **Phase 1**: 紐づけ（明示と候補の提示）、取得、GUI 表示、審査への事実の供給。
- **Phase 2**: CI 失敗の通知。手動送信と、PR ごとの auto fix による自動通知（終了済み run の wake を含む）。旧案の Phase 3（全体設定による自動再開）は Phase 2 に統合した。

### GUI

最新の情報と操作（「agent に送る」、auto fix トグル）は画面下部に置く。mock で 3 案を比べ、案 A を採る。

- **PR バー**: 会話の入力欄（composer）の直上に置く。折りたたみ時は 1 行で、展開すると上方向に伸びる。CI 失敗時は展開し、auto fix が動作中は畳む。check の一覧、head SHA、マージ状態、Link / 更新 / 「agent に送る」の操作、auto fix トグルを持つ。
  - PR はスレッド単位なので、subagent の transcript を開いている間も PR バーを表示する。
- **スレッド一覧**: 行に PR バッジ（#番号と ✓ / ✗ / ● / merged）を出す。
- **候補 PR**: `gh pr create` の出力の直下にチップ（Link PR / 無視）を出す。手動の「Link PR…」は、スレッド名の右クリックメニューに加える。
- **ハーネスの通知**: transcript では `harness` タグ付きのカードにし、ユーザーの発言や agent のメッセージと区別する。CI ログの抜粋は「外部データ — 信頼しないこと」と明記した囲みに入れる。
- **Notifications**: CI failed、Merged、auto fix の上限到達を加える。
- **テーマ**: merged 用に紫を加える（Graphite と Tokyo Night の両方）。
- 細部（ピクセル、色の値）は実装時に決める。

採らなかった案:

- 案 B（専用の PR パネル）: 会話から視線が離れ、Subagents と同時に見えない。必要になれば、PR バーを補う詳細パネルとして将来加える余地はある。
- 案 C（Diff タブへの統合）: 既定の左カラム幅に収まらず、Notifications と同じタブ群に入るため隠れる。

## Consequences

- 紐づけがあるスレッドの審査は、PR・CI・ローカル HEAD の状態を根拠にできる。その分、拒否の一部（状態が見えないことによる拒否）が減ると期待する。実際の減少は未計測。
- `gh` の認証状態に依存する。未認証や失敗は Diagnostic にし、事実は stale または不在として扱う。
- ポーリングの取得頻度は GitHub API の rate limit に影響する。間隔と backoff で抑える。
- 新テーブルと、runtime の定期取得、審査入力の項目、GUI の画面が増える。
- 自動通知は agent の実行を増やす。歯止め（SHA 単位の 1 回、回数上限、cooldown、`AUTO_WAKE_LIMIT`）と PR ごとの既定オフで抑える。

## Alternatives

- **審査時だけ都度取得し、追跡しない最小案**: 実装は小さいが、GUI 表示と CI 失敗の通知に使えず、審査のたびに外部呼び出しの待ちが入る。
- **agent に `gh` を実行させ、自己申告させる**: agent の主張は検証できず、審査役に渡せる事実にならない。
- **Webhook / GitHub App**: ローカル実行の evorch に公開エンドポイントと App の導入を要求する。トークン・秘密の管理も増える。
- **agent 専用ツール `pr_link` / `pr_status`**: ToolSpec の途中追加が cache 契約に反する。紐づけは agent の主張になり、検証が要る。
- **GitHub API を直接呼ぶ**: トークンを evorch が保持する必要がある。`gh` の認証に任せる。

## 未決事項

- マージ後に、派生スレッドへ候補として PR を出すか。
- 通知の回数上限と cooldown の既定値（叩き台は 3 回・10 分）。
- 外部 push と思われる head の判定方法。
- 複数ディレクトリがあるときの、PR とチェックアウトの照合方法の詳細。
- auto fix を全体で無効化するスイッチを持つか。
- レビューコメントを通知の対象に含めるか。

## 外部の参考

- Copilot CLI `/pr fix ci`: 利用者の操作が起点。
- Cursor Cloud Agents: 自分で作った PR の Actions 失敗に条件付きで対応する。人の push や base でも落ちる check には追従しない。
- Conductor: Checks ビュー。
- Devin: CI Error Resolution のテンプレート。
- Orca（stablyai/orca）: PR を worktree に紐づけて扱う。CI 失敗から agent を起動する案は issue #10131 にあり、opt-in・cooldown・絞り込み付きの提案段階。
- t3code: CI の監視から agent を再開する issue のみ。
- Claude Desktop の auto fix: 利用者から聞いた挙動として記録する。公開資料での確認は未了。

## Validation and limits

- 未実施: 実装、テスト、cache 契約の検証。実装時は、通知が末尾追記のみであることを `scripts/check-cache-contracts.sh` の e2e シナリオに加えて確かめる。
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
