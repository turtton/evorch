# ADR 0044: lane を独立した Orchestrator thread として管理する

## Status

Proposed。

利用者が合意した lane の基本要件、報告ポリシー、lead の lifecycle 独立、表示・family 操作、Orchestrator delegate の全面廃止を具体化する草案。Phase 3 に関するユーザー決定と実装時に確定する技術事項は末尾に列挙する。

## Context

Orchestrator が多数の Worker や子 Orchestrator を管理すると、進捗、質問、作業履歴、完了報告が親の会話に集まり、文脈が膨張する。人間も、5〜10以上の thread を個別に巡回しなければ問題を把握しにくくなる。

既存機構には次の性質がある。

- `delegate` は同一 thread 内の child run を作る。現在は Orchestrator も target に指定できる。
- 現行 `escalate` は root Worker が自身を終了させ、新しい Orchestrator thread に仕事を引き継ぐ（[ADR 0033](0033-file-tool-path-boundary.md)）。
- thread は独自の会話、goal（[ADR 0028](0028-thread-goals-and-completion-checks.md)）、ownership、再開履歴を持つ。
- `thread_read` 等により、必要なときだけ別 thread の詳細を読める。
- sidebar は入れ子表示を持ち、pin/archive は現在 family 単位で管理する。
- 相談役（Advisor）と審査役（Reviewer）の境界（[ADR 0035](0035-advisor-consultation-boundary.md)、[ADR 0040](0040-advisor-stuck-point-triggers.md)）: Advisor は助言のみで受入判定を行わず、Reviewer が仕様適合と品質を判定する。
- キャッシュ契約（[ADR 0003](0003-cache-first-context-engine.md)、`AGENTS.md`）: 通常ターンでは送信済みの system・tools・入力 prefix を変えない。

詳細を独立 thread に置き、管理側には短い構造化 Report と要対応事項だけを届ける単位として **lane** を導入する。

dsh-superagent の構造化 Report・要対応中心の表示、dori-superagent の brief・完了申告と検証の分離・無報告検知を参考にする。ただし、専用の常駐 lead ロールや固定工程は作らない。

## Decision

### 1. lane と lead

lane は、**lane 外の root Orchestrator が開く独立 thread**である。

- lane の root は通常の Orchestrator。
- 作成元 Orchestrator を便宜上 **lead** と呼ぶ。新しい Role ではない。
- lane は独自の会話と goal を持ち、利用者が直接開いて対話できる。
- 未完了 goal は既存どおり thread ごとに1つ（[ADR 0028](0028-thread-goals-and-completion-checks.md)）。
- lane を使わない従来の作業・delegate も維持する。

管理上の扱いを次のように分ける。

| policy | 管理上の扱い |
|---|---|
| attached | lead が管理する lane |
| summary | lead が管理する lane。通常報告を終了時に絞る |
| detached | lead との連携を切った独立 thread。lane close の管理対象外 |

detached にも作成経緯と immutable な実行 scope は残すが、それらは現在の管理権限ではない。

### 2. Orchestrator delegate を全面廃止する

`delegate.target.role` から **Orchestrator を常に除外する**。

- root Orchestrator、lane Orchestrator のどちらからも Orchestrator を delegate できない。
- schema、モデル引数の parser、child 起動認可の各層で拒否する。
- `Role::Orchestrator` 自体は残す。
- **`delegate_background(Role::Orchestrator, …)` の `parent=None` による root 起動は維持する。**
- 拒否するのはモデルの delegate target と、`parent=Some(...)` の Orchestrator child 起動である。
- lane Orchestrator は Planner・Worker・Reviewer 等を束ねる最下層の調整役となる。

既存の nested Orchestrator delegate の用途は lane に置換する。

### 3. lane のネストを禁止する

lane 作成ツールは、lane 外の root Orchestrator にだけ公開する。

- lane root の tool schema から `lane_open` を除外する。
- root ではない run にも作成を許可しない。
- runtime 側でも同じ制約を強制する。
- lane scope は runtime が設定・永続化し、モデル引数で変更できない。
- detach 後も lane 作成能力は追加しない。
- restore、fork、rewind、モデル切替で制約を失わない。

各 run の tool set は起動時に固定する。policy 変更や detach で schema を増減させない。

### 4. lead と lane の lifecycle を独立させる

lane root の `Run.parent` は `None` とする。管理関係は別の永続レコードで表現し、通常の subagent 実行木へ混ぜない。

不変条件は次のとおり。

- lead の再開・停止・終了は、lane の実行、質問、未終結 ready によってブロックされない。
- lead の Stop は lane を暗黙停止しない。
- lane の live run や pending admission を、lead の restore conflict に含めない。
- lane の未回答質問を、lead の直接 child の finish gate に含めない。
- lead が終了しても lane は作業を続けられる。
- 一括停止を提供する場合は、通常の Stop と別の明示操作にする。

lead 自身の二重実行防止、ownership、未確定の副作用に関する安全確認は維持する。lane の存在を拒否理由にしない、という契約である。

### 5. brief を最初の入力として渡す

```text
LaneBrief
  objective
  inputs[]                 # 説明、thread/message/artifact ref
  deliverables[]
  acceptance_criteria[]
  constraints[]
  workspace_declaration
    branch?
    worktree?
    notes?
  report_policy            # attached | summary | detached
```

- objective と acceptance criteria は必須。
- 入力なし等の場合は空配列を明示できる。
- branch/worktree は宣言のみ。作成、checkout、予約、排他制御はしない。
- 宣言されたパスによって sandbox 権限を広げない。
- **brief は system prompt ではなく、最初の user-role 入力として渡す。** ただし由来は agent の brief と明示し、実ユーザーの依頼・実行許可には昇格させない。
- 後の補足、再 brief、policy 変更は会話末尾に追加する。
- 必要な元依頼と制約は、既存 provenance 経路で保持・参照する。

#### 補足（`lane_send`）と再 brief（`lane_revise_brief`）

開始済みの管理下 lane（`attached` / `summary`）に対して lead から後から指示を送る tool として、`lane_send` と `lane_revise_brief` を設ける（schema は run 内で固定し、lane 側および非 root には公開しない。migration は v59）。

- **`lane_send`（補足）**: 現在の brief に対する補足（最大 4 KiB）を送る操作であり、brief revision を上げない。既存の Ready や検証・close の根拠を失効させない。
- **`lane_revise_brief`（再 brief）**: brief を全面改訂する操作（差分パッチではなく全フィールドを送る。`report_policy` は現在の policy と一致させる）であり、変更があれば brief revision を +1 上げる（同一内容は no-op）。revision が上がると既存の検証・close の根拠は失効する。
- **会話末尾への追記と由来の明示**: 補足も改訂も、lane の会話の末尾に追記する（cache 契約）。本文には lead Orchestrator（agent）が出した指示であることを明示し、実ユーザーの依頼・実行許可には昇格させない。
- **認可**: 呼び出せるのは現在の lead root（lane 外の root Orchestrator）だけとする。実行主体は dispatcher が付与し、detached な lane・lane 自身・子 run・旧 lead は `not_current_lead` で拒否する。
- **全文保存と receipt**: 認可、`expected_management_revision` / `expected_brief_revision` の CAS、brief 更新、不変の指示全文（`lane_instructions`）の保存、outbox（`Instruction { instruction_seq }`）への配送予約、および操作 receipt（`lane_instruction_receipts`）の発行を 1 つの IMMEDIATE transaction で行う。指示は受理した時点で全文を保存し、そのあと receipt を発行する（全文保存前に receipt を出さない。同一 `operation_id` の再送は receipt から元の結果を replay し、別要求への再利用は `operation_id_conflict` で拒否する）。送信元 thread が削除されても、受理済みの本文は lane が残る限り保持する。
- **配送順序**: 配送は lane 内連番（`instruction_seq`）および revision の順に行う（未配送の最小連番を 1 件ずつ claim し、会話 snapshot と配送 receipt を同時に保存する）。
- **detach 前後の扱い**: 受理済みの指示は、その後 detach（または再 attach）された後も同じ lane へ配送する（management revision は配送時に re-stamp する）。detach 後に旧 lead が送ったものは認可・revision fence で拒否する。
- **wake と Stop**: `Done` または `Error` の lane は、既存の永続 wake guard 付きで起こす（guard が suppressed の間は解除まで保留し、免除しない）。明示的に Stop されている lane では配送を保留する。
- **Ready に記録する観測済み brief revision**: Ready を含む Report には、その時点で lane の会話が実際に観測した（配送 receipt 済みの）brief revision を記録する（未配送の改訂がなければ現在の revision、あれば最も古い未配送改訂の改訂前 revision）。まだ受け取っていない改訂に対して Ready を出しても古い revision として扱われ、§9 の `stale_brief` で close の根拠にならない。

### 6. 構造化 Report と会話外台帳

lead に戻す作業情報は、短い構造化 Report に限定する。

```text
LaneReport
  lane_id
  report_seq               # runtime 採番
  status                   # working | waiting | blocked | ready | failed
  summary
  artifact_refs[]
  needs_human
  question_refs[]
  reason_code?
  producer                 # lane agent / harness
```

| status | 意味 |
|---|---|
| working | 作業中 |
| waiting | 回答・外部条件等の明示した待機 |
| blocked | 自律的には進めない |
| ready | 成果が揃ったという申告 |
| failed | 実行・復帰等の失敗 |

`ready`、run の `Done`、goal 完了、lane close（台帳上の終結）は別概念とする。

台帳には少なくとも以下を保存する。

- lane/thread ID、immutable な作成元 thread/run
- 現在の管理 lead、management revision
- 現在の root run、実行 generation、lane scope
- brief と revision、policy
- Report 履歴と latest snapshot
- close 記録と任意の検証（Reviewer）記録・対象 revision
- 配送先・理由・wake 要否・ACK
- stall/nudge/wake 抑止 episode

推奨初期上限は Report 全体 4 KiB、summary 1 KiB、成果物 ref 8 件。超過を黙って切り捨てず、修正可能なサイズエラーにする。

#### `lane_list` の契約

- 現在管理する lane を、**最新の実活動時刻の降順、同値なら lane ID 順**で返す。
- 1ページ最大 20 件かつ 16 KiB。
- `next_cursor`、`has_more`、snapshot revision を返す。
- cursor は filter と snapshot に結び付ける。同じ snapshot のページ間で重複・欠落を起こさない。
- 保持期間切れ等は `stale_cursor` とし、一覧の取り直しを要求する。
- 各ページで現在の読取権限を再確認する。
- 読取・監視 tick・再描画だけで更新順を変えない。

台帳全体を毎ターン prompt に注入しない。詳細は必要時に `thread_read` 等で読む。

detached 化後は lead 向け Report を要求・配送しない。管理終了の記録は残すが、通常 thread の実行状態は既存経路で扱う。

### 7. 報告ポリシーと質問先

| policy | 通常報告 | 質問先 | lane close |
|---|---|---|---|
| attached | 節目＋終了時短報 | まず lead。権限外はユーザー | 必要 |
| summary | 終了時短報のみ | ユーザー | 必要 |
| detached | lead への報告なし | ユーザー | 対象外 |

終了時短報は ready または失敗終了であり、成功 close を意味しない。

attached lane 内の子 run の質問は、まず lane root が扱う。解決できない質問だけを lead に送る。質問 ID、元 requester、回答者 provenance は維持する。

lead の回答権限は、現在の管理関係、revision、ownership で検証する。agent 回答は実ユーザーの実行許可ではない。

ユーザーが attached/summary lane に直接話しかけた場合、その入力を lane に届け、lead には短い介入通知と参照を届ける。本文を lead 側の実ユーザー依頼として複製しない。detached の介入は lead に通知しない。

#### 質問 32 件制限との関係

現在の storage は、作成 run と継承リンクを基準に、requester ごとの**未回答**質問を 32 件に制限する（回答すると枠が空く）。全体の未回答数の上限は設けない。`user_question_links` は継承であり、回答者の受信箱ではない。

lane では次を採用する。

- lead へのルーティングを理由に、質問を lead の `user_question_links` に bind しない。
- 元 requester が質問を所有し続ける。
- 別の lane 質問ルーティング記録に、質問 ID、管理 lead、management revision、配送状態を保存する。
- lead は paginated な lane 質問一覧で読む。**lead 配下全体に 32 件の受信上限は設けない。**
- lead 自身の質問と lane 由来質問を、`user_answers` や finish gate で混ぜない。
- ユーザー判断が必要になっても、lead が質問を作り直さず、元質問の回答先をユーザーへ切り替える。

requester ごとの未回答 32 件制限は、過剰生成のガードとして維持する（全体未回答 1024 件の上限は撤廃した）。到達時は質問作成を明示的に失敗させ、受理済み質問を削除しない。lane に bounded な `question_capacity_blocked` 診断を残し、GUI に表示する。自動再試行ループは行わない。

#### protocol DTO への写像可能性（Phase 1c 制約）

GUI/core 分離および Web（スマホ）操作の作業（thread-9gkyvvvf）を見据え、Phase 1c の次の情報は runtime 内部型に閉じず、共通 protocol の DTO に当てはめられる形（serde 可能な ID と列挙型）で定義する。

- **質問の宛先記録（responder routing）**: 宛先（ユーザー / lead）、management revision、routing revision
- **元の requester**: run・thread・root run
- **回答者**: 人か agent か。将来「desktop / web」の区別を追加予定であるため、列挙型は拡張可能（non-exhaustive 等）に定義する。

### 8. 終了済み lead への配送・wake・暴走抑止

#### 宛先と wake 要否

| lead 宛て情報 | lead が実行中・待機中 | lead が通常終了済み |
|---|---|---|
| attached の質問 | ターン境界で配送 | wake して配送 |
| failed | 配送 | wake して配送 |
| ready | 配送 | wake して配送 |
| working 等の節目 | policy に従い配送 | 保留。次ターンに集約。単独では wake しない |
| 情報としての介入通知 | bounded に配送 | 原則保留 |

summary の質問はユーザー宛てなので、それだけでは lead を wake しない。detached から lead への配送はない。

利用者が明示 Stop した lead は自動 wake させない。通知は保留する。ownership 不在、restore 失敗、暴走抑止も通知を失う理由にしない。

#### `AUTO_WAKE_LIMIT = 20` とは別の経路を使う

**lane 由来の重要通知は、既存 `take_auto_wake` の累積 20 回枠を消費しない。** 通常 subagent 等の既存経路とその上限は変更しない。通常 delegation の質問に追加した直接の子ごとの別枠は [ADR 0042 の補足](0042-event-driven-waits.md#補足-通常-delegation-の質問通知と親の-wake) を参照（通常 send／完了通知の既存枠は不変）。

理由は、正常な多数 lane の終了や質問が、ユーザー入力なしで 20 回を超えること自体は暴走ではないためである。既存枠を単に増やす方式は採用しない。

代わりに Phase 1c から以下を実装する。

1. **永続 outbox**
   受理した重要通知を先に保存する。

2. **意味に基づく重複排除**
   キーは lane、management revision、イベント種別、質問 ID／成果物 revision／失敗 episode 等。Report sequence の増加や同じ状態の再送だけで別の重要イベントにしない。

3. **lead 単位の single-flight wake**
   未配送通知を lane×イベント単位で束ね、1 つの wake 要求にする。同じ lead の復元を重複実行しない。大量の質問本文を一括注入せず、短い Report と永続 inbox の参照・件数を届ける。

4. **累積量ではなく、無進展の往復を抑止（リセット信号の限定）**
   発生元 lane ごとに、実進展を伴わない因果的な wake 往復を追跡する。初期値は 20 回の連続した無進展 wake batch とする。これは会話全体の総回数上限ではない。
   リセット信号は以下に限定する：
   (a) lead または対象 lane への実ユーザー入力（`reset_auto_wakes` 相当）
   (b) ready または failed に伴う成果物 revision の変化
   (c) ユーザーによる明示的な再開操作
   **新規質問 ID の生成は進展に数えない**（質問→回答→新規質問の往復ループを抑止するため）。通知 ACK、heartbeat、Report sequence の増加、状態の往復、agent 同士の返信でもリセットしない。

5. **抑止は発生元単位**
   1 lane のループによって、別 lane の新しい ready や質問を止めない。

6. **停止時も durable に保留**
   抑止された通知は残し、GUI に理由・対象 lane・再開操作を表示する。手動再開や実進展で再評価する。明示 Stop は手動解除なしに解除しない。

この設計により、独立した正常イベントが続く限り、ユーザー入力をリセット条件にせず 20 回以上の wake を処理できる。一方、同じ情報の再配送や無進展の相互起動を止められる。

#### ACK、claim lease、crash recovery

「wake 要求を発行した」だけで配送済みにしない。

- runtime が batch を claim する（claim lease を設定）。
- host が Stop・ownership・generation を再確認し、root を復元する。
- 受信側が安定 ID 付き通知を永続的な入力として受け入れる。
- その結果を ACK する。
- **host が処理できない場合（`thread_of_run` が不明、他 GUI が所有する `readonly_threads` 等）は、明示的に NACK を返し lease を解放する。**
- 未 ACK / 解放済み batch の再駆動契機は、(a) thread ownership 取得時、(b) runtime 起動時の未 ACK 走査、(c) 当該 thread へのユーザー入力時、とする。

復元や ACK の前後で crash しても、通知が消えず、同じ通知 ID を会話へ二重追加しない。既に送った provider prefix は変更しない。

#### 単一コア構成の検討と wake 往復の実装順序（Phase 1c 制約）

[ADR 0024](0024-process-owner-claim.md)（ownership）を廃止し「コアを 1 つだけ起動し GUI と Web はそこへ接続する」構成が議論されている（thread-9gkyvvvf）。この構成が採用されると、wake → root 復元 → ACK の往復と、他 GUI 所有 thread に対する NACK は GUI ではなくコア内で完結する。

方針確定までの間、GUI 側の wake 往復（`frame.rs` / `actions.rs` / `restoration.rs` 等）の作り込みは最小限（コアから呼べる入口程度）にとどめ、runtime/storage 側（durable outbox、重複排除、受領記録、無進展往復 guard）を先行実装・検証する。

### 9. 完了申告・任意レビュー・終結

attached/summary の `ready` は完了申告であり、それ自体は台帳上の終結（close）ではない。close は成果物の物理的な不変性や品質のシステム保証ではなく、**lead（または人間）の管理上の判断**として記録する。

#### 変更の理由（運用で得た教訓）

commit hash（固定成果物 revision）、clean な checkout、Fresh な PR facts、独立 Reviewer の承認・正常終了をすべて揃えるシステム的な検証 gate は、`main` の進行や dirty な状態、worktree の削除、多数の thread の並行に弱く、実運用で lane が close できなくなった（`thread-mma4g2qa`、`thread-mbjqn30f`、`thread-x4awbhy9`）。そのため、システムで不変性を保証して close を止めるのではなく、close を lead の判断に戻し、検証や観測結果は補助情報として添える方針に変えた。

#### close の必須条件（管理上の正しさだけ）

lead による `lane_close` の必須条件は次の5つだけとする。

- 現在の lead root であること（dispatcher 由来の実行主体を close transaction の中で `threads.root_run_id` と照合する）
- attached または summary であること
- management／request／brief revision が一致すること
- 最新の Ready であること（現在の brief より古い brief 向けの Ready は `stale_brief` で拒否し、警告では済ませない）
- detach ではないこと（detach は管理関係の終了であり、close ではない）

#### close を止めないものと close 記録（参考情報の添付）

artifact revision の有無・一致、clean な checkout、PR facts（[ADR 0043](0043-pull-request-integration.md)）の Fresh 状態、Reviewer の承認・正常終了は close を止めない。`lane_close` は Git・GitHub・hash・Reviewer の run を一切呼ばない（再観測しない）。

`lane_closes` には種別（`lead_closed` / `human_closed` / `discarded`。`verified_closed` は作らない）とともに、参考情報として次を添付・記録する。

- **Ready の参照と revision**: 対象 `ready_seq`、Ready で報告されたままの `artifact_refs` / `artifact_revision`（close 時に再検証しない）、Ready が報告された `report_brief_revision`、および close 時点の `management_revision`・`request_revision`（`closed_revision`）・`brief_revision`。
- **PR context（`pr_context`）**: close 時点で台帳が持つ PR の保存済み状態（`not_linked` / `no_snapshot` / `last_fetch_failed` / `available`（evidence つき））。レビューを省略した close にも付き、台帳からの読み出しだけで外部取得も gate もしない。
- **レビューの派生状態（`review`、任意添付）**: 同じ lane の `verification_ref` を指定した場合、その時点の導出 `state`（`pending` / `submitted` / `completed` / `incomplete`）、`verdict`、結果参照・digest、`targets_this_ready`、`pr_evidence`、開始時の観測限界 `caveats` を添付する。`artifact_rechecked_at_close` は常に `false`。同じ lane の記録であれば任意の state を添付でき、別 lane や不明な ID は拒否する。
- **`closed_by` の provenance（変更不能）**: lead は認可に使った thread と root run、人間は host が認証した surface と client。tool 引数やメッセージ本文からは決めず、runtime / host の provenance から埋める。
- **lead／人間のメモ（`note`）**: 判断理由（必須・最大 2 KiB）。

`close_id` は `(lane, kind, management_revision, request_revision, ready_seq)` から決まる。同じ引数の再送は同じ `close_id` と `already_closed` を返し、行も通知も増やさない（最初の close の記録が残る）。lane への新しい入力、新しい Report、brief／management の変更があれば現在の close は解除されて `request_revision` が進み、古い引数の再送は `stale_request` 等で拒否して現在の revision（`current`）を返す。

#### `lane_verify`（任意の補助）

- `lane_verify` は任意の判断補助とする。Ready に固定参照（フル commit SHA、PR URL、または 64 MiB 以下の単一通常ファイルの SHA-256 等）を付けるのは推奨にとどめ、必須にしない。
- 仕様適合・品質の独立判定自体は Reviewer が行う（[ADR 0035](0035-advisor-consultation-boundary.md)、[ADR 0040](0040-advisor-stuck-point-triggers.md)）。CI 成功や Advisor の助言で受入判定は代替しないが、レビューの実施・承認は close の可否を決めない。PR のない調査・設計・文書作業にも対応する。
- 成果物の観測や PR facts の取得に失敗・不一致があっても Reviewer は起動し、観測限界を `caveats` として記録して Reviewer に渡す。
- Reviewer の提出（`submit_lane_verification`）は開始時の brief の acceptance criteria 件数に対して検証し、審査中に lane が先に進んでいても結果を捨てない。
- Reviewer が `Done` 以外（失敗・取消）で終了したとき、または `Done` でも提出がないときは verification を `Incomplete` にする（不合格 `Rejected` とは区別する。operator の Stop は再開できるためそのまま）。書き戻し前に落ちた場合も、attention や close 添付時の状態は Reviewer 自身の run 記録（`run_contexts.terminal_phase`）から導出するため「実行中」のまま残らない（読み出し自体は状態を変更しない）。

#### 人間による close と破棄

- 人間による close（`human_closed`）と破棄（`discarded`）は、**台帳上の終結だけ**を行う（run の停止、thread／worktree の削除、detach は別の操作。PR #566）。
- verified close（設けない）や `lead_closed` とは区別して記録する（人間は Ready がない状態でも終結できる）。
- 実行経路は、認証済みの人間が使う wire 経路（`Command::CloseLane`、namespace `lanes/close_lane`、host が provenance を付与）と、GUI の Close…／Discard… の確認画面（理由 `note` の入力が必須、成果物の参照等を参考情報として表示）から実行できる。agent の tool、MCP、dispatch からは到達できない（信頼された host 経路 `HumanLaneManagement::close_lane_as`、`Actor::Human` のみ許可。PR #566）。

detached はこの管理から外れる。既存 goal checks や通常のレビュー運用は使えるが、元 lead による lane close を要求しない。detach は close ではなく、管理関係の終了である。

### 10. sidebar・family

**attached/summary**

- lead 配下に入れ子表示する。
- 配下の lane は最新更新順。
- lane 単体の pin/archive は持たない。
- 現行 family と同じく lead の pin/archive 操作に従う。

**detached**

- トップレベルの独立 thread。
- 独立した pin/archive を持つ。
- detached の間は通常 thread と同様に自由に pin/archive できる。detach 直後は未 pin・未 archive で開始し、lead family の状態を引き継がない。
- 元 lead の archive に巻き込まれない。
- detach 時も thread ID、会話、goal を維持し、表示位置と family 所属だけを変更する。
- 再 attach はユーザー操作のみとし、サイドバーで lane を lane 外の root Orchestrator へドラッグ＆ドロップして行う。lead からの一方的な再 attach はできず、lane 外 root Orchestrator 以外へのドロップは拒否する。
- pin または archive 中の detached lane は再 attach できない。ドロップ時に理由を表示して拒否し、先に pin/archive を解除する。
- 再 attach 後は managed lane として lead family に従い、単体 pin/archive を持たない。

immutable な作成元と、現在の表示親・管理親を分離する。GUI、mirror、retention は同じ有効 family 解決規則を使う。古い sidebar 保存で管理関係を復活させない。

### 11. 監視と要対応一覧

監視は常駐 LLM ではなく、runtime のイベントと永続状態から行う。

次を区別する。

- 無報告
- 実作業の停滞
- 質問・外部障害による待機
- 再起動後の復帰失敗

長い tool 実行、回答待ち、利用者 Stop、goal checks Pause（[ADR 0028](0028-thread-goals-and-completion-checks.md)）を故障と混同しない。summary は内部実行状態から監視し、報告頻度の少なさ自体を stall としない。

stall episode ごとに nudge を重複抑制し、同じ状態に無期限反復しない。nudge は権限を与えず、会話末尾に追加する。

横断一覧には、管理対象 lane の未解決質問、blocked、failed、復帰失敗、未終結 Ready（`ReadyUnclosed { ready_seq, readiness }`）を表示する。通常の working 更新は警告にしない。

Ready なのに close されていない状態（`ReadyUnclosed`）は、作業の stall とは混同せず、`readiness`（`LaneReadyState`）で細分化する。

- `no_evidence`: Ready に固定成果物 revision がなく、独立レビューもない
- `unreviewed`: 固定成果物 revision はあるが、独立レビューがない
- `review_in_progress`: Reviewer が起動され、まだ完了していない
- `review_rejected`: 完了したレビューが Ready を不合格（`rejected`）とした
- `review_incomplete`: Reviewer が提出完了せずに終了した（失敗・取消・停止・無提出終了）。不合格とは区別し、再度 Verify を選べる
- `awaiting_close`: レビューが承認（`approved`）済みで、lead がまだ close していない

いずれの `readiness` も参考情報であり、close を妨げない（Close は常に操作選択肢に入り、`review_in_progress`・`awaiting_close` 以外では Verify も提示する）。[ADR 0043](0043-pull-request-integration.md) の PR facts の Fresh 定義は維持するが、lane では強制 gate ではなく状態つきの参考情報（`pr_context` / `pr_evidence` と `caveats`）として使う。

detached の質問・障害は通常 thread と同等に扱い、lead 通知、lane close 待ち、lead 向け監視には戻さない。

### 12. delegate・lane・handoff の使い分け

| 手段 | 選ぶ条件 |
|---|---|
| delegate | 単一ロールで完結し、lead がその場で結果を受け取り、必要な Reviewer 判定等に基づいて合否・採否を確定できる作業 |
| lane_open | 計画→実装→レビュー等のロール横断調整、PR 等の独立成果物、長期作業、ユーザーとの直接対話が見込まれる作業 |
| 現行 escalate／統合後の handoff | root Worker が自分の実行を終え、Orchestrator に仕事を引き継ぐ場合 |

経験則は **「以前なら Orchestrator に delegate していたものは lane」** とする。nested Orchestrator delegate は選択肢として残さない。

### 13. 資源上限と NoProgress

#### lane root

時間、ターン数、token、累積 tool call 数による上限を設けない。使用量は計測する。

Phase 1b では、runtime 内部の予算型（`BudgetSettings` 等）で `ResourceLimit<T> { Unlimited, Bounded(T) }` を導入し、lane scope 起動時に無制限を適用する。未指定（既定値適用）と無制限の取り違えは runtime 型で防止する。operator 向けの `BudgetConfig`（config schema、JSON schema、strict 検査）への露出は必要になった時点で別途行う。NoProgress、IdenticalToolCalls 等の暴走診断は維持する。

**lead である通常 root Orchestrator の既定予算は変更しない。** 現在の 400 tool calls、実稼働時間 2 時間、既定 token 上限なしを維持する。通常 child の設定も、この変更で一律に無制限にはしない。

#### lane_open と spawn credit

lane は child run ではないため、`mark_child_spawned` を呼ぶだけにして completion 回収を欠落させてはいけない。

Phase 1b で、既存の「未回収の起動に対する進捗 credit」を child/lane の識別付き共通契約へ拡張する。

- 実際に新規起動に成功した child/lane は 1 度だけ credit を使う。
- 作成失敗、冪等再試行、一覧の再読込は credit を得ない。
- 同じ管理主体における child と lane の起動は、共通の未回収起動列として扱う。交互起動による回避を許さない。
- 新しい terminal 結果を初めてモデルが観測したときに進捗として扱い、credit を回復する。
- lane では初回の有効な ready/failed の観測が対応する。単なる Report 作成ではなく、`lane_list` の応答または通知配送でモデルに届いたことを基準にする。
- 同じ結果の再取得、heartbeat、新しい sequence だけでは回復しない。
- detach は完了観測とみなさない。

初期 credit 数は既存の 20 を再利用する。21 個目以降の lane 作成を拒否する上限ではなく、それ以降の起動だけでは NoProgress をリセットしない、という診断である。

同時 lane 数の固定上限は設けない。必要性が実測された場合にだけ検討する。

#### 未回収の lane 進捗と新規作成の一時抑止（Phase 4B）

共通 spawn credit とは独立して、完成済み成果を未回収のまま継続する新規 lane 作成を診断する。未回収とは、現在の lead の管理下にあり、latest Report が現行 brief/management revision の Ready または Failed で、モデルへの初回提示を観測していない lane である。基準となる未回収 lane 集合が残る間に成立した新規作成 intent を identity ごとに一度数え、初期閾値100に達した後の新規 lane_open だけを一時抑止する。既存 intent の冪等再試行、既存 lane の実行・回収・回答は続行する。

成果が provider に提示された場合は当該 lane のみを基準集合から除き、集合が解消されたら連続作成区間を終了する。管理終了した lane も対象から除くが、detach 自体は completion 観測や共通 credit 回復ではない。別 lane の completion、Report 採番・heartbeat・通常 tool は古い集合と回数をリセットしない。running の数や経過時間だけでは抑止しない。

これは lane 総数・同時数の上限ではなく、共通 credit 20 とその全回復の仕様は維持する。初期値100と20の比較は4Eで評価し、既定値を20へ変更するにはユーザーの追加確認を要する。

既知の制約として、intent を永続化してから計数を保存するまでの間に crash すると、その未保存分の計数は補修されず、抑止が遅れる（テストシナリオでは1件。PR #564）。

また、未回収作成の抑止や復帰停止は診断コードとして追跡・通知する（Phase 4D D1、PR #571）。未回収作成抑止時は `LaneUncollectedCreationSuppressed`（HarnessImprovement）を、自動復帰が3回失敗して停止したときは `LaneRestoreRetrySuppressed`（TransientOrExternal）を発行する。いずれも再試行や復元で重複発行せず、cohort（または recovery episode）ごとに1回だけ発行する。

### 14. workspace

workspace 衝突をシステムで強制排除しない（[ADR 0031](0031-remove-isolated-workspaces.md)）。

branch/worktree 宣言を一覧で比較できるようにし、実際の分離は agent の既存 worktree 運用に任せる。宣言は予約・排他・権限付与ではない。

### 15. 永続化・復帰・detach

通常の lane_open は lead の goal や ownership を移転せず、lane 用の独立 ownership を取得する。

```text
brief 検証
→ 作成 intent・lane ID・起動 seed 保存
→ ownership / admission
→ lane root 起動
→ GUI 投影
```

作成再試行は冪等にし、GUI 保存前の crash で lane を失わない。保存失敗時は成功を返さない。

Phase 1b（PR #477）で GUI 起動経路に入れた `recover_unstarted_lanes` の呼び出しと、lane root の command sink 登録（`bind_lane_root`）は、単一コア構成（thread-9gkyvvvf、[ADR 0024](0024-process-owner-claim.md) 廃止検討）になった場合はコア側へ移す対象である。

既存 thread 全体を SQL 正本へ移す変更は行わない。ただし lane の存在、管理関係、policy、実行管理は台帳を正とし、sidebar はその投影を持つ。title 等の既存 GUI 管理属性は host が管理する。

attached↔summary の変更は run を作り直さずに行う。

detach は management revision を更新し、以下を一つの管理遷移として処理する。

- lead の管理権限終了
- 未配送 Report・wake の失効
- 未回答質問のユーザー宛て切替
- 進行中の検証や現在の close 状態の管理上の失効
- トップレベル・独立 family への移動
- lane 専用監視・close 待ちからの除外

遅延した旧 lead の回答・close は拒否する。送信済み会話は改変しない。

再 attach はユーザー操作のみで行う。GUI サイドバーで detached lane を lane 外の root Orchestrator へドラッグ＆ドロップする方式とし、lead からの一方的な操作や他の宛先へのドロップは拒否する。pin または archive 中の lane は、理由を表示してドロップを拒否し、先に解除する。再 attach 後の report policy は `summary` を既定とし、後から変更できる。

再 attach でも detach と対称に management revision を更新する。stale な GUI 保存と同時 detach/attach の競合は revision で拒否・解決し、旧管理関係に基づく操作を受け付けない。

### 16. escalation の統合

公開名は次を推奨する。

- `lane_open`：Orchestrator が lane を開き、自身は継続。
- `handoff_to_lane`：root Worker が detached な Orchestrator thread へ移管し、自身は終了。

共通の起動・保存基盤を使うが、後者だけが必要な goal、未回答質問、継続情報を移す。

引継ぎ先は最初から detached のトップレベル thread であり、元 Worker に close 責任を残さない。準備に失敗した場合、元 Worker を終了させない。

作業移管には `handoff` を用い、sandbox 権限昇格と区別する。`escalation_review` を lane の完了検証へ一括流用・改称しない。

### 17. cache と旧 run

- system、tools/schema/order、model/settings、cache affinity key は run 内で安定させる（[ADR 0003](0003-cache-first-context-engine.md)、`AGENTS.md`）。
- Report、回答、nudge、policy 変更は末尾追記とする。
- detach で schema を変更しない。使用不能になった操作は runtime の状態エラーにする。
- 送信済み Report を後から集約・置換しない。集約できるのは未配送分だけ。
- compaction、role-chain switch 以外の暗黙の履歴置換を導入しない。

#### 旧 run の復元契約

- **拒否対象**: `role == Orchestrator && parent.is_some()` である保存済みの nested child Orchestrator run の restore のみ、理由付きで明示的に拒否する。
- **root Orchestrator の復元**: 既存の root Orchestrator thread は通常どおり復元し、新 schema で継続する。version 境界での一度の prefix 変更は従来どおり許容され、run 内の安定性（cache 契約）には抵触しない。
- 旧 nested run を自動的に lane へ変換しない。
- v0.x の互換性方針（`AGENTS.md`）に従い、復元互換層や大規模な移行基盤は作らない。

## Consequences

- lead の文脈量を lane の詳細な実行量から切り離せる。
- lead の終了・再開を lane から独立させられる。
- 全 thread の巡回を、要対応中心の管理へ寄せられる。
- 同一 thread 内で、モデルの Orchestrator delegate により**root→子 Orchestrator→孫 Worker という 3 段以上の run 木を作る経路はなくなる**。
- root Orchestrator 起動、通常の 2 段の delegate、汎用の保存・復元データ構造は維持する。
- nested Orchestrator 前提の prompt・テスト・運用は変更が必要。
- durable outbox と host による root 復元の往復が必要になる。
- detached では lead による lane close 管理を受けず、ユーザーとの通常 thread 運用に戻る。
- family 移動と複数 GUI の保存競合を扱う必要がある。
- workspace 衝突と短い Report による情報不足は完全には防げない。
- 10+ lane の正常イベントを、既存 wake の累積 20 回で止めない一方、無進展の往復を別途診断する。

## Alternatives

- **nested Orchestrator delegate を残す**：調整経路が二重化するため不採用。
- **lane を child run とする**：親の停止・復元・完了 gate と結合するため不採用。
- **専用常駐 lead ロール**：通常 Orchestrator を維持する要件に反する。
- **全会話の継続要約を lead へ送る**：文脈量が実行量に比例する。
- **ready をそのまま完了（close）とする**：完了申告と lead の終結判断（および任意の独立レビュー）を分離できない。
- **detached も close 管理する**：独立 thread という意味に反する。
- **AUTO_WAKE_LIMIT を単に増やす**：正常な長期運用と暴走の区別にならない。
- **lane 質問をすべて lead に継承する**：32 件制限・finish gate・provenance を誤用する。
- **固定 lane 数上限・workspace lock を先行導入する**：現時点で必要性の根拠がない。

## 未決事項と決定済み事項

### Phase 3 に関するユーザー決定（解消済み）

| 項目 | 決定 |
|---|---|---|
| detached からの再接続 | ユーザーがサイドバーで lane を lane 外の root Orchestrator へドラッグ＆ドロップして行う。lead の一方的な再 attach と他の宛先へのドロップは不可。pin/archive 中は理由を表示して拒否し、先に解除する。再 attach 後の既定 policy は `summary` とし、後から変更できる |
| detached 中と detach 直後の pin/archive | detached 中は通常 thread 同様に自由に pin/archive できる。detach 直後は未 pin・未 archive で開始し、lead 側の状態は引き継がない |

### 実装時に確定する技術事項

| 項目 | 推奨・境界 |
|---|---|
| fork/rewind 後の管理対象 | lane 作成不可 scope を維持。台帳が追う現行 root/version を一つに定める。未対応操作は明示拒否 |
| stall/nudge 閾値 | configurable、episode 単位。作業時間制限にはしない |
| PR/CI 証拠 adapter | [ADR 0043](0043-pull-request-integration.md) の Fresh 定義を維持し、lane では強制 gate ではなく状態つきの参考情報（`pr_context` / `pr_evidence` と `caveats`）として使う |
| 人間による close／破棄の範囲 | 台帳上の終結（`human_closed` / `discarded`）のみを行い、run の停止、thread／worktree の削除、detach は別操作とする（GUI 操作画面の接続時にユーザー最終確認） |
| 未回収 lane 進捗の抑止閾値 | Phase 4B の初期閾値は 100。初期値 100 と 20 の比較は Phase 4E で評価し、既定値を 20 へ変更するにはユーザーの追加確認を要する |
| cursor 保持期間 | bounded な snapshot 保持と明示的 `stale_cursor`。現在の権限検査は省略しない |
| 単一コア構成への移行 | [ADR 0024](0024-process-owner-claim.md) 廃止・コア集約（thread-9gkyvvvf）確定時の、`recover_unstarted_lanes` / `bind_lane_root` や wake 往復・NACK 処理のコア側移管 |
| 導入タイミング | operator/orchestrator が旧 run の完了・停止状況を確認して決定 |

## Validation and limits

- 未実施: 実装、テスト、cache 契約の検証。
- 段階的実装計画（`docs/lane-phased-implementation-plan.md`）に記載された各 Phase の受け入れ条件および cache e2e 検証に従って検証を行う。

## Related

- [ADR 0003](0003-cache-first-context-engine.md): キャッシュ第一コンテキストエンジン（run 内安定性）。
- [ADR 0022](0022-parent-child-tree-addressing-and-nested-delegation.md): 親子・派生スレッドのアドレス指定。
- [ADR 0024](0024-process-owner-claim.md): thread process owner / claim モデル（単一コア構成による廃止検討、thread-9gkyvvvf 参照）。
- [ADR 0028](0028-thread-goals-and-completion-checks.md): スレッドゴールと完了チェック。
- [ADR 0031](0031-remove-isolated-workspaces.md): workspace 衝突はシステム強制せず宣言と運用で解決。
- [ADR 0033](0033-file-tool-path-boundary.md): ファイルパス境界と escalation 審査。
- [ADR 0035](0035-advisor-consultation-boundary.md)、[ADR 0040](0040-advisor-stuck-point-triggers.md): 相談役（Advisor）と審査役（Reviewer）の境界。
- [ADR 0042](0042-event-driven-waits.md): イベント駆動待機。
- [ADR 0043](0043-pull-request-integration.md): PR 状態取得と審査証拠。
- [段階的実装計画](../../../docs/lane-phased-implementation-plan.md): Phase 1a〜Phase 4 の詳細計画。
