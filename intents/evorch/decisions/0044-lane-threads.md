# ADR 0044: lane を独立した Orchestrator thread として管理する

## Status

Proposed。

利用者が合意した lane の基本要件、報告ポリシー、lead の lifecycle 独立、表示・family 操作、Orchestrator delegate の全面廃止を具体化する草案。未決事項（ユーザー確認待ち）は末尾に列挙する。

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

- lead の再開・停止・終了は、lane の実行、質問、未検証 ready によってブロックされない。
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

`ready`、run の `Done`、goal 完了、検証済み lane close は別概念とする。

台帳には少なくとも以下を保存する。

- lane/thread ID、immutable な作成元 thread/run
- 現在の管理 lead、management revision
- 現在の root run、実行 generation、lane scope
- brief と revision、policy
- Report 履歴と latest snapshot
- 検証対象 revision・検証記録
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

現在の storage は、作成 run と継承リンクを基準に 32 件を制限し、全体の未回答数を 1024 件に制限する。`user_question_links` は継承であり、回答者の受信箱ではない。

lane では次を採用する。

- lead へのルーティングを理由に、質問を lead の `user_question_links` に bind しない。
- 元 requester が質問を所有し続ける。
- 別の lane 質問ルーティング記録に、質問 ID、管理 lead、management revision、配送状態を保存する。
- lead は paginated な lane 質問一覧で読む。**lead 配下全体に 32 件の受信上限は設けない。**
- lead 自身の質問と lane 由来質問を、`user_answers` や finish gate で混ぜない。
- ユーザー判断が必要になっても、lead が質問を作り直さず、元質問の回答先をユーザーへ切り替える。

既存 requester の 32 件制限と全体未回答 1024 件は、保存容量・過剰生成のガードとして維持する。到達時は質問作成を明示的に失敗させ、受理済み質問を削除しない。lane に bounded な `question_capacity_blocked` 診断を残し、GUI に表示する。自動再試行ループは行わない。

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

**lane 由来の重要通知は、既存 `take_auto_wake` の累積 20 回枠を消費しない。** 通常 subagent 等の既存経路とその上限は変更しない。

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

### 9. 完了申告と検証

attached/summary の ready は申告であり、検証済み完了ではない。

lead は brief の完了条件に対応する証拠を確認し、close する。

- 仕様適合・品質の独立判定は Reviewer が行う（[ADR 0035](0035-advisor-consultation-boundary.md)、[ADR 0040](0040-advisor-stuck-point-triggers.md)）。
- PR/CI は完了条件に含まれる場合だけ使う（[ADR 0043](0043-pull-request-integration.md)）。
- CI 成功だけで品質の受入判定を代替しない。
- PR のない調査・設計・文書作業にも対応する。
- Advisor の助言は受入判定ではない。
- lead は Reviewer の判定に基づき、採否と close を統括する。

検証記録は ready sequence、brief revision、成果物 revision／commit SHA に結び付ける。追加依頼・成果変更で古くなった記録は close に使えない。

detached はこの管理から外れる。既存 goal checks や通常のレビュー運用は使えるが、元 lead による lane close を要求しない。

detach は成功 close ではなく、管理関係の終了である。

### 10. sidebar・family

**attached/summary**

- lead 配下に入れ子表示する。
- 配下の lane は最新更新順。
- lane 単体の pin/archive は持たない。
- 現行 family と同じく lead の pin/archive 操作に従う。

**detached**

- トップレベルの独立 thread。
- 独立した pin/archive を持つ。
- 元 lead の archive に巻き込まれない。
- detach 時も thread ID、会話、goal を維持し、表示位置と family 所属だけを変更する。

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

横断一覧には、管理対象 lane の未解決質問、blocked、failed、復帰失敗、未検証 ready を表示する。通常の working 更新は警告にしない。

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

既存 thread 全体を SQL 正本へ移す変更は行わない。ただし lane の存在、管理関係、policy、実行管理は台帳を正とし、sidebar はその投影を持つ。title 等の既存 GUI 管理属性は host が管理する。

attached↔summary の変更は run を作り直さずに行う。

detach は management revision を更新し、以下を一つの管理遷移として処理する。

- lead の管理権限終了
- 未配送 Report・wake の失効
- 未回答質問のユーザー宛て切替
- close 検証の管理上の失効
- トップレベル・独立 family への移動
- lane 専用監視・close 待ちからの除外

遅延した旧 lead の回答・close は拒否する。送信済み会話は改変しない。

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
- detached では lead の完了検証を受けず、ユーザーとの通常 thread 運用に戻る。
- family 移動と複数 GUI の保存競合を扱う必要がある。
- workspace 衝突と短い Report による情報不足は完全には防げない。
- 10+ lane の正常イベントを、既存 wake の累積 20 回で止めない一方、無進展の往復を別途診断する。

## Alternatives

- **nested Orchestrator delegate を残す**：調整経路が二重化するため不採用。
- **lane を child run とする**：親の停止・復元・完了 gate と結合するため不採用。
- **専用常駐 lead ロール**：通常 Orchestrator を維持する要件に反する。
- **全会話の継続要約を lead へ送る**：文脈量が実行量に比例する。
- **ready を完了とする**：自己申告と検証を分離できない。
- **detached も close 管理する**：独立 thread という意味に反する。
- **AUTO_WAKE_LIMIT を単に増やす**：正常な長期運用と暴走の区別にならない。
- **lane 質問をすべて lead に継承する**：32 件制限・finish gate・provenance を誤用する。
- **固定 lane 数上限・workspace lock を先行導入する**：現時点で必要性の根拠がない。

## 未決事項

### ユーザー確認待ち（Phase 3 前に解消）

| 項目 | 推奨案 | 影響 |
|---|---|---|
| detached からの再接続 | ユーザーの明示操作のみ。旧 lead の一方的な再 attach は不可 | 親エージェント判断で推奨案を採用。ユーザー未確認のため、Phase 3 着手前に確認する。未確定なら再接続機能を提供しない |
| detach 時の pin/archive 初期値 | 遷移時点の有効状態を snapshot として独立 family へ移す | 常に未 archive・未 pin で独立させる案との UX 差があるため、Phase 3 前に確認する |

### 実装時に確定する技術事項

| 項目 | 推奨・境界 |
|---|---|
| fork/rewind 後の管理対象 | lane 作成不可 scope を維持。台帳が追う現行 root/version を一つに定める。未対応操作は明示拒否 |
| stall/nudge 閾値 | configurable、episode 単位。作業時間制限にはしない |
| PR/CI 証拠 adapter | [ADR 0043](0043-pull-request-integration.md) の実装状況を再確認。未完成なら独立 Reviewer 証拠を先行 |
| cursor 保持期間 | bounded な snapshot 保持と明示的 `stale_cursor`。現在の権限検査は省略しない |
| 導入タイミング | operator/orchestrator が旧 run の完了・停止状況を確認して決定 |

## Validation and limits

- 未実施: 実装、テスト、cache 契約の検証。
- 段階的実装計画（`docs/lane-phased-implementation-plan.md`）に記載された各 Phase の受け入れ条件および cache e2e 検証に従って検証を行う。

## Related

- [ADR 0003](0003-cache-first-context-engine.md): キャッシュ第一コンテキストエンジン（run 内安定性）。
- [ADR 0022](0022-parent-child-tree-addressing-and-nested-delegation.md): 親子・派生スレッドのアドレス指定。
- [ADR 0028](0028-thread-goals-and-completion-checks.md): スレッドゴールと完了チェック。
- [ADR 0031](0031-remove-isolated-workspaces.md): workspace 衝突はシステム強制せず宣言と運用で解決。
- [ADR 0033](0033-file-tool-path-boundary.md): ファイルパス境界と escalation 審査。
- [ADR 0035](0035-advisor-consultation-boundary.md)、[ADR 0040](0040-advisor-stuck-point-triggers.md): 相談役（Advisor）と審査役（Reviewer）の境界。
- [ADR 0042](0042-event-driven-waits.md): イベント駆動待機。
- [ADR 0043](0043-pull-request-integration.md): PR 状態取得と審査証拠。
- [段階的実装計画](../../../docs/lane-phased-implementation-plan.md): Phase 1a〜Phase 4 の詳細計画。
