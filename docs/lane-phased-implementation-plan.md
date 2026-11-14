# lane 段階的実装計画（ADR 0044 実装ロードマップ）

本文書は [ADR 0044: lane を独立した Orchestrator thread として管理する](../intents/evorch/decisions/0044-lane-threads.md) の段階的実装計画である。

## 1. Phase とレビュー境界

| 段階 | 内容 | レビュー境界・公開契約 |
|---|---|---|
| **1a** | Orchestrator delegate 除外、テスト・案内・旧 run の扱い | delegate schema からの除外、nested child 復元の拒否。中間段階であり preview |
| **1b** | lane_open、台帳、Report、lane_list、入れ子／初期 detached 表示、予算型、spawn credit | 質問は暫定で全 policy ユーザー宛て、自動 wake なし。中間段階（preview） |
| **1c** | attached 質問の lead 配送、durable outbox、選択的 wake、介入通知、10+ lane 対応 | Phase 1 完成形。10+ lane・20 回超 wake・40 質問対応 |
| **2** | 横断要対応、stall/nudge、検証付き close | 自律的 stall 検出、要対応 panel、Reviewer 判定等に基づく検証 close |
| **3** | policy 変更、detach と family 移動、escalation 統合 | policy 動的変更、同一 thread の top-level family 移動、Worker handoff 統合 |
| **4** | 暴走診断の整理・緩和、必要なら同時数制限 | 長期自律運用のための診断整理、無進展・無成果作成の抑止 |

1a・1b はレビュー可能な中間段階であり、Phase 1 完成とは表示しない。1b の暫定質問先・wake 不在を最終仕様として案内しない。通常配布を 1c までまとめるか、中間版を明示的な preview とするかは orchestrator が決める。

いずれの段階でも、稼働中 run の schema や routing 契約を hot swap する導入は行わない。

## 2. 担当と編集競合回避

| 担当 | 所有範囲 |
|---|---|
| **A：永続契約** | storage、migration、共有 DTO・イベント、revision・冪等性 |
| **B：runtime/tool/policy/予算** | 起動、scope、delegate 除外、認可、budget 型、NoProgress 接続 |
| **C：質問・配送往復** | 質問先、outbox、wake guard、runtime→GUI 復元→ACK、Stop 再確認 |
| **D：GUI/family** | sidebar、表示順、直接対話表示、pin/archive、family・retention 投影 |
| **E：統合・cache** | e2e、障害注入、wire-prefix、contract filter |
| **F：文書** | ADR、関連方針、移行案内、完成段階の説明 |

共有ファイルは担当を固定する。

- migration 採番、`event.rs`：A
- `runtime.rs`、`meta/registry.rs`、`policy.rs` の統合配線：B
- **`frame.rs`、`actions.rs`、wake に関係する `restoration.rs`：C**（ただし単一コア構成検討に伴い GUI 作り込みは最小限）
- sidebar/family、GUI の一般モジュール登録：D
- cache テスト入口・filter：E

C が **durable outbox → root 復元 → ACK の全往復** を統合責任として持つ。D に wake の半分だけを暗黙に委ねない。B/C/D が共有ファイルを同時編集しないよう、追加モジュールに処理を置き、配線を所有者へ渡す。なお、[ADR 0024](../intents/evorch/decisions/0024-process-owner-claim.md)（ownership）廃止および単一コア構成（thread-9gkyvvvf）が議論中であるため、GUI 側の wake 往復の作り込みは最小限（コアから呼べる入口程度）とし、runtime/storage 側（durable outbox、重複排除、受領記録、無進展往復 guard）の検証を先行させる（GUI 側往復は方針確定後に調整）。

---

## Phase 1a — Orchestrator delegate の除外

### 変更対象

- `crates/runtime/src/meta/delegation.rs`
- `crates/runtime/src/meta/specs.rs`
- `crates/runtime/src/runtime.rs`
- `crates/runtime/src/policy.rs`
- `crates/agents/src/role.rs`
- `crates/runtime/src/prompt/key_triggers.rs`
- `crates/runtime/src/prompt/catalog.rs`
- `crates/config/assets/presets/role-orchestrator.md`
- 関連する role・schema・委譲テスト

### 内容

B が `DELEGATE_ROLES`、parser、child admission/spawn 経路を変更する。alias 等からの経路にも同じ認可を適用する。

`delegate_background` の `parent=None` は変更しない。`spawn_reserved` 等の下位経路でも、通常実行における Orchestrator child の迂回を許さない。

名指しするテスト:
- `crates/runtime/tests/delegation/parallel_delegates.rs`: `awaited_waiting_child_keeps_attention_snapshot_when_user_input_arrives` は現在、モデルが Orchestrator を delegate して孫 Worker を作る。1a では直接 child の attention/input 契約へ整理し、lane とその Worker の独立性は 1b/1c で別途検証する。
- `crates/runtime/tests/request_turn_context.rs`
- `crates/runtime/src/meta/role_tests.rs`
- `crates/runtime/tests/system_prompt_seam.rs`
- `crates/runtime/tests/delegation/delegation_progress.rs`
- `crates/runtime/src/runtime/completion_relay/tests.rs`
- `crates/runtime/tests/restore_state/restore/` 配下

root 起動に使う `delegate_background(Role::Orchestrator, …)` を一括削除しない。`chat_restore/renewal.rs` 等の汎用 deep-tree unit test は、モデルから許される木を表すテストと区別する。必要なら test-only fixture で保持し、production の認可迂回を残さない。

### 受け入れ条件

- delegate schema に Orchestrator が含まれない。
- parser / API 経由の Orchestrator child 起動（`parent.is_some()`）を明示的に拒否する。
- root Orchestrator は通常どおり起動・restore できる。
- **旧 run 復元契約**: `role == Orchestrator && parent.is_some()` の保存済み child Orchestrator run の restore は理由付きで明示的に拒否される。一方、既存の root Orchestrator thread は通常どおり復元し、新 schema で継続する（version 境界での一度の prefix 変更は従来どおり許容され、run 内の安定性には抵触しない）。
- nested 専用テストを置換し、cancel・attention・provenance 等の検証目的を失わない。
- 起動済み run の schema を途中で変更しない。

### cache e2e

通常 root Orchestrator → Worker delegate の複数ターンで、schema 内容・順序と入力 prefix を保持する。削除後の schema の snapshot 更新だけで済ませず、provider wire で確認する。

---

## Phase 1b — lane 基盤、予算型、表示

### A：永続化

既存対象:
- `crates/storage/src/migrations.rs`
- `crates/storage/src/threads.rs`
- `crates/storage/src/repo/threads.rs`
- `crates/storage/src/writer.rs`
- `crates/storage/src/read.rs`
- `crates/storage/src/lib.rs`
- `crates/event-bus/src/event.rs`
- `crates/event-bus/src/fencing.rs`

新設候補:
- `crates/storage/src/migrations/v39.sql`（採番は PR 作成時点で最新を確認）
- `crates/storage/src/lanes.rs`
- `crates/storage/src/repo/lanes.rs`

保存するもの:
- lane identity、出自、管理関係、scope、brief、policy
- Report と最新 snapshot
- 作成要求の冪等キー、起動 seed、generation
- 1c で使用する配送契約のキー・revision

全 outbox 実装は 1c だが、1b で Report 履歴を失わないようにする。

### B：runtime と予算

既存対象:
- `crates/runtime/src/runtime.rs`
- `crates/runtime/src/run.rs`
- `crates/runtime/src/run_store.rs`
- `crates/runtime/src/thread_lineage.rs`
- `crates/runtime/src/restore.rs`
- `crates/runtime/src/runtime/chat_restore/renewal.rs`
- `crates/runtime/src/ownership/host.rs`
- `crates/runtime/src/agent_loop.rs`
- `crates/runtime/src/policy.rs`
- `crates/runtime/src/meta/mod.rs`
- `crates/runtime/src/meta/registry.rs`
- `crates/runtime/src/meta/specs.rs`
- `crates/runtime/src/budget_tracker.rs`
- `crates/runtime/src/budget_tracker_tests.rs`
- `crates/runtime/src/agent_loop/budget.rs`

新設候補:
- `crates/runtime/src/lanes.rs`
- `crates/runtime/src/meta/lanes.rs`

予算と資源上限の設計方針:
- runtime 内部の予算型（`BudgetSettings` 等）で `ResourceLimit<T> { Unlimited, Bounded(T) }` を導入し、lane scope 起動時に無制限を適用する。
- 「未指定（既定値適用）」と「無制限」の取り違えは runtime の型定義で担保する。
- operator 向けの `BudgetConfig`（`crates/config/src/types/budget.rs`, `strict.rs`, `config/tests/settings_suite/budget.rs`）への露出は 1b では行わず、将来必要になった時点で別途検討する（Phase 1b の変更対象から config 系ファイルを除外し、担当 B の責務を縮小）。
- 通常 lead の既定値（400 calls、2 時間、token 上限なし）および既存明示設定は維持する。
- usage 計測・NoProgress・IdenticalToolCalls は維持する。
- child/lane 共通の spawn credit と、lane terminal の初回観測を接続する。

### D：GUI/family

既存対象:
- `crates/workspace-ui/src/thread.rs`
- `crates/workspace-ui/src/lineage.rs`
- `crates/workspace-ui/src/family.rs`
- `crates/workspace-ui/src/merge.rs`
- `crates/workspace-ui/src/sidebar.rs`
- `crates/gui/src/app/thread_mirror.rs`
- `crates/gui/src/app/shared_sidebar.rs`
- `crates/gui/src/panes/sidebar/threads.rs`

新設候補：`crates/gui/src/app/lanes.rs`。

- attached/summary を入れ子・更新順で表示。
- lane 単体 pin/archive を出さない。
- 初期 detached は最初からトップレベル・独立 family。
- 直接対話、brief・policy・workspace 宣言の表示。
- 台帳から sidebar の欠落を補修。
- 出自（immutable）と可変管理関係を明確に分離する。

### 暫定動作（preview）

1b では全 policy の質問をユーザーへ届け、lane 由来の自動 wake・介入通知は行わない。lead は `lane_list` で Report を取得する。UI・文書で preview の制限を明記する。

### 依存順序

DTO/API 確定 → A → B/D 並列 → E 統合。B の予算型変更は、lane 起動の完成を待たず独立テストできる。

### 受け入れ条件

- lead が継続したまま複数の独立 root lane を開ける。
- lane の実行・admission・質問で lead の Stop/Finish/Resume がブロックされない。
- lane root に `lane_open` がなく、restore 後も増えない。
- 3 policy の初期表示・family が契約どおり。
- `lane_list` の更新順（最新実活動降順）、サイズ上限（16 KiB/20 件）、cursor、同一 snapshot の完全性を検証。保持期限切れは `stale_cursor` を返す。
- 再試行・GUI 保存前 crash で二重起動・lane 消失がない。
- 1b では質問は必ずユーザー宛てで、wake しない。
- lane root は 400 calls や 2 時間を超えても、それだけでは停止しない。
- lead の既定予算と NoProgress/IdenticalToolCalls は従来どおり。
- 21 個目の lane を数だけで拒否しない。一方、無成果の連続作成で無期限に NoProgress をリセットしない。
- 初回 terminal 観測で credit が戻り、同じ Report の再読込では戻らない。
- ready を未検証として扱う。

### cache e2e

`lane_open`、brief の初回 user-role 入力、`lane_report`、`lane_list` の複数ページ、Report 更新後の再読込、lane/lead それぞれの通常ターン・restore。過去の tool result の再編集がないことを確認する。

---

## Phase 1c — 質問配送、durable wake、介入通知

### 設計制約（単一コア構成・GUI/core 分離・Web 操作対応）

thread-9gkyvvvf（GUI/core 分離・Web（スマホ）操作）および [ADR 0024](../intents/evorch/decisions/0024-process-owner-claim.md)（ownership）廃止・コア一元化の議論を踏まえ、Phase 1c に以下の制約を適用する。

1. **protocol DTO への写像可能性**:
   次の情報は runtime 内部型に閉じず、共通 protocol の DTO に当てはめられる形（serde 可能な ID と列挙型）で定義する。
   - 質問の宛先記録（responder routing）: 宛先（ユーザー / lead）、management revision、routing revision
   - 元の requester: run・thread・root run
   - 回答者: 人か agent か。将来「desktop / web」の区別を追加予定であるため、列挙型は拡張可能（non-exhaustive 等）に定義する。
2. **wake 往復の実装順序**:
   ADR 0024 廃止・単一コア構成が採用された場合、wake → root 復元 → ACK の往復と、他 GUI 所有 thread に対する NACK は GUI ではなくコア内で完結する。方針確定までの間、GUI 側の wake 往復（`frame.rs` / `actions.rs` / `restoration.rs` 等）の作り込みは最小限（コアから呼べる入口程度）にとどめ、runtime/storage 側（durable outbox、重複排除、受領記録、無進展往復 guard）を先行実装・検証する。GUI 側の全往復統合はコア構成の方針確定後に調整する。

### C の変更対象

- `crates/runtime/src/meta/questions.rs`
- `crates/runtime/src/runtime/questions.rs`
- `crates/runtime/src/runtime/questions/continuation.rs`
- `crates/runtime/src/runtime/held_messages.rs`
- `crates/runtime/src/mailbox.rs`
- `crates/storage/src/repo/user_questions.rs`
- 1b の lane storage/runtime モジュール
- **`crates/gui/src/app/frame.rs`**（単一コア構成確定までは最小限の接続）
- **`crates/gui/src/app/actions.rs`**（単一コア構成確定までは最小限の接続）
- `crates/gui/src/app/restoration.rs`
- `crates/gui/src/app/questions.rs`
- `crates/gui/src/app/composer.rs`

現行の root wake は `frame.rs` の `RootWakeRequested` 処理から `actions.rs::wake_conversation_root` へ進む。この往復に batch ID、management revision、Stop/ownership 再確認、claim lease、ACK を接続する。ただし単一コア構成が議論中であるため、GUI 側往復の作り込みは最小限とし、runtime/storage 側で検証を先行する。

C が処理全体を所有し、A が transaction/event 契約、B が runtime 配線を支援する。

### Tool Schema の変更（1b → 1c）

1b → 1c で root Orchestrator の tool schema に `lane_questions` / `lane_answer` などの質問管理 tool が追加される。1b から 1c への移行において root Orchestrator の tool schema が変化することは、バージョン境界（preview 境界）としての変更であり許容される（cache 契約上は run 内の安定性のみが必須要件）。

### 内容

- attached の質問を lead へ配送。他 2 policy はユーザーへ。
- responder routing と continuation inheritance を分離。
- 元 ID のままユーザーへ判断を送れる経路。
- outbox、重複排除、single-flight wake。
- **無進展往復 guard のリセット信号限定**: (a) lead/lane への実ユーザー入力、(b) ready/failed に伴う成果物 revision の変化、(c) ユーザーの明示再開、に限定。**新規質問 ID は進展に数えない**。
- 通常終了済み lead は質問・failed・ready で wake。
- working 等は保留し、次ターンに bounded に集約。
- runtime と host の両方で Stop を確認。
- **claim lease と NACK/解放**: host が処理できない場合（`thread_of_run` 不明、他 GUI 所有の `readonly_threads` 等）は明示的に NACK/lease 解放を行う（単一コア構成ではコア内で完結予定）。
- **再駆動契機**: thread ownership 取得時、runtime 起動時の未 ACK 走査、ユーザー入力時。
- 入力の永続受領と ACK、crash repair。
- 直接対話の介入通知。

### 受け入れ条件

1. **10 lane 同時通知**: バリアで同じ配送周期に 10 lane の重要イベントを発生させ、全イベントが保存され、lead 復元は 1 つだけになる。本文は bounded で、全件を参照・取得できる。
2. **20 回超の正常 wake**: ユーザー入力を挟まず、独立した進展を伴う 25 回以上の通知・終了・再開を処理する。既存 `AUTO_WAKE_LIMIT` を消費せず、既に通常枠が 20 に達した lead でも lane の正当な通知を処理できる。
3. **lead 受信 32 件超**: 10 lane が各 4 件、計 40 件の質問を作る。元 requester は各 4 件、lead は全 40 件をページング取得・回答できる。lead 自身が既に 32 件の質問を持つ場合も lane の responder inbox と衝突しない。
4. **容量ガード**: requester の未回答 32 件の境界で明示エラーとなり（回答すると枠が空く。全体上限なし）、既存質問が消えない。容量不足診断が残り、自動再試行ループにならない。
5. **暴走抑止**: 同じイベント再送、sequence だけの増加、無進展の質問・通知往復を検出する。抑止対象の通知は durable に残り、別 lane の正当な ready は配送される。
6. **Stop と復元の競合**: wake 発行後・復元前に Stop しても再開しない。既に停止した lead の通知は保留される。単一コア完結の可能性を踏まえ、まずは runtime/storage 側で検証し、GUI 側往復は方針確定後に調整する。
7. **crash と ACK**: claim 後、復元後、入力保存後、ACK 前後で障害を注入し、通知消失・同一 ID の二重 append・二重 root がない。claim lease タイムアウトや処理不能時の NACK/再駆動が動作する。単一コア完結の可能性を踏まえ、まずは runtime/storage 側で検証し、GUI 側往復は方針確定後に調整する。
8. **完了非ブロック**: lead に未処理 lane 質問や未検証 ready があっても finish/resume できる。配送済みの同じ質問だけで繰り返し wake しない。

### cache e2e

質問の作成・回答・ユーザーへの宛先変更、重要通知 batch、終了済み lead の wake、介入通知、途中経過の次ターン集約、ACK 再試行、Stop 後の明示再開。

---

## Phase 2 — 要対応・stall/nudge・検証付き close

> **改訂注記（PR #559）**: Phase 2C の厳格な検証ゲート付き close は改訂され、close は管理上の正しさ（現在の lead root、attached/summary、management/request/brief revision の一致、現在 brief 向けの最新 Ready、非 detach）のみを必須とする lead の判断に戻された。`lane_verify`（独立 Reviewer）や成果物 revision・PR facts は任意の参考情報として close 記録に添付し、close を止めない（[ADR 0044 §9](../intents/evorch/decisions/0044-lane-threads.md#9-完了申告任意レビュー終結)・[§11](../intents/evorch/decisions/0044-lane-threads.md#11-監視と要対応一覧)）。

### 変更対象

1b/1c の lane モジュールに加えて:
- `crates/runtime/src/thread_goals.rs`
- `crates/runtime/src/thread_goals/boundary.rs`
- `crates/runtime/src/restore.rs`
- `crates/gui/src/app/attention.rs`
- `crates/gui/src/model/notifications.rs`
- `crates/gui/src/panes/notifications.rs`
- 必要なら `crates/workspace-ui/src/panels.rs`

### 内容・担当

- A：検証記録、stall episode、未解決状態。
- B：イベント／clock 駆動の monitor・nudge。
- C：lane_close と証拠照合。
- D：横断一覧、通常 thread 通知との区別。

ready sequence、brief/artifact revision、Reviewer 結果を照合し、古い証拠を使わせない。PR/CI は [ADR 0043](../intents/evorch/decisions/0043-pull-request-integration.md) の実装状況を再確認して接続し、完成済み前提にはしない。

detached は lane close・lane 専用管理一覧から除外する。

### 受け入れ条件

- 仮想時間で stall を検出し、episode ごとに nudge が重複しない。
- 正当な待機、長時間 tool、Stop、summary を誤検知しない。
- 再起動・複数プロセスで通知を重複させない。
- ready／Done／goal 完了だけでは close しない。
- 古い SHA・brief・Report、不合格 Reviewer 結果で close できない。
- 検証中の追加依頼で古い判定が失効する。
- PR がない作業も完了できる。
- detached に close を要求せず、旧 lead 操作は拒否する。
- ready 待ちがあっても lead lifecycle を止めない。

**cache e2e 必須**：nudge、検証差戻し、失敗通知、wake 後の prefix 保持。

---

## Phase 3 — policy 変更・detach・handoff 統合

### A. policy と family 遷移

対象:
- lane runtime/storage/GUI モジュール
- `crates/workspace-ui/src/family.rs`
- `crates/workspace-ui/src/merge.rs`
- `crates/workspace-ui/src/lineage.rs`
- `crates/gui/src/app/shared_sidebar.rs`
- `crates/gui/src/app/thread_mirror.rs`
- `crates/gui/src/app/thread_archive.rs`
- `crates/gui/src/app/thread_retention.rs`
- `crates/storage/src/thread_retention.rs`

実装内容:
- attached↔summary の動的変更。
- detach・再 attach 時の管理 revision 更新。
- 質問宛先変更、未配送通知失効、旧 lead 回答・close 拒否。
- 進行中検証を成功扱いせず管理終了。
- 同一 thread のトップレベル・独立 family 移動。
- 古い sidebar 保存による再所属を防止。
- lane 専用監視から通常 thread 扱いへ切替。
- detached lane は通常 thread 同様に pin/archive できる。detach 直後は未 pin・未 archive で開始し、lead 側の状態を引き継がない。
- 再 attach はユーザー操作のみとし、サイドバーで detached lane を lane 外の root Orchestrator へドラッグ＆ドロップする。他の宛先へのドロップと lead からの一方的な再 attach は拒否する。
- pin/archive 中の detached lane は、理由を表示してドロップを拒否する。再 attach 前に pin/archive を解除する。
- 再 attach 後の report policy は `summary` を既定とし、後から変更可能にする。

DB と GUI ファイルを一つの transaction にできると仮定しない。台帳の遷移を確定し、revision 付き投影を冪等に補修する。不確かな中間状態では retention を保守的に止める。

**ユーザー決定済み事項**：
1. 再 attach はユーザーがサイドバーで lane を lane 外の root Orchestrator へドラッグ＆ドロップして行う。lead からの一方的な再 attach と他の宛先へのドロップは不可。pin/archive 中は理由を表示して拒否し、先に解除する。再 attach 後の既定 policy は `summary` とし、後から変更できる。
2. detached 中は通常 thread 同様に自由に pin/archive できる。detach 直後は未 pin・未 archive で開始し、lead 側の状態は引き継がない。

### B. escalation 統合（`handoff_to_lane`）

対象:
- `crates/runtime/src/meta/escalation.rs`
- `crates/runtime/src/escalation/mod.rs`
- `crates/runtime/src/escalation/handoff.rs`
- `crates/runtime/src/escalation/prompt.rs`
- `crates/runtime/src/runtime.rs`
- `crates/runtime/src/restore/escalation.rs`
- `crates/runtime/src/thread_goals.rs`
- `crates/runtime/src/continuity.rs`
- `crates/agents/src/role.rs`
- `crates/gui/src/app/escalation.rs`
- `docs/harness-runtime-improvements.md`

保持する契約:
- 準備失敗で元 Worker を終了しない。
- durable seed、質問継承、ownership 準備後に source terminal を公開。
- 起動失敗時も復帰情報を残す。
- goal、元依頼、条件、Pause、使用量履歴、質問 ID を保持。
- 引継ぎだけで blocked goal を解除しない。
- 古い review 判定を新 root の完了根拠にしない。
- 引継ぎ先は最初から detached・独立 top-level で、lane close 不要。
- 通常 `lane_open` は lead を終了させない。

### 受け入れ条件

- policy 変更・detach で schema/order を変更しない。
- 古い lead 宛 wake・回答・close を revision で拒否。
- ユーザーがサイドバーで detached lane を lane 外の root Orchestrator へドラッグ＆ドロップすると再 attach でき、その他の宛先は拒否される。
- lead の一方的な再 attach はできず、pin/archive 中の lane は理由を表示して拒否され、解除後に再 attach できる。
- detach 直後は未 pin・未 archive で、detached 中は pin/archive が可能。
- 再 attach 後の既定 policy は `summary` であり、後から変更できる。
- 再 attach でも detach と対称に management revision を更新し、stale GUI 保存や同時 detach/attach の競合を revision で拒否・解決する。
- 未回答質問の ID/requester を維持。
- ready、回答、検証完了、archive との競合で二重所属・誤 close がない。
- thread・会話・goal・成果物を保ったまま表示移動。
- detach 後は lead archive の影響を受けない。
- managed のままなら lead family に従う。
- Worker handoff の成功／準備失敗／起動失敗が契約どおり。

**cache e2e 必須**：policy 変更、detach、失効した旧管理操作、handoff、継承質問回答、復帰後 prefix。

---

## Phase 4 — 暴走診断の整理

対象:
- `crates/runtime/src/budget_tracker.rs`
- `crates/runtime/src/budget_tracker_tests.rs`
- `crates/runtime/src/agent_loop/budget.rs`
- `crates/runtime/src/agent_loop/identical_calls.rs`
- `crates/runtime/src/escalation/detector.rs`
- `crates/runtime/src/runtime/held_messages.rs`
- lane wake guard と spawn credit の実装

Phase 1 で必要な無制限型・wake guard・spawn credit は既に実装済みとし、Phase 4 へ先送りしない。

ここでは以下を整理する:
- 同一失敗・同一 tool 呼出しの反復。
- 同じ一覧・Report の再読込。
- **成果を回収しない連続作成**: 共通 spawn credit は「初回 completion で全回復（`unobserved_spawns = 0`）」する仕様であるため、成果を回収しない連続作成の診断においては、**lane ごとの未回収を別に数える**。
- nudge／Report／wake の相互ループ。
- 同じ復帰失敗の反復。
- 長期調整で正当な待機・再読込を誤検知する閾値の調整。

### 受け入れ条件

- 10 以上の模擬 lane の正常な調整を暴走扱いしない。
- lane root を時間・turn・token・tool 総数だけで止めない。
- 無成果作成・同一失敗・通知ループは bounded に抑止できる。
- heartbeat や Report 採番で診断を回避できない。
- 新しい実進展で正しく再評価する。
- 固定同時数上限は、実測の根拠がなければ導入しない。

---

## 3. 旧 run と導入手順

推奨手順:
1. 更新前 runtime で、実行中 tool と外部副作用を確認。
2. 旧 nested Orchestrator を安全に完了、または Stop・保存。
3. 新版で保存済み nested child Orchestrator（`role == Orchestrator && parent.is_some()`）は自動再開せず理由付きで拒否する。
4. 既存の root Orchestrator thread は通常どおり復元し、新 schema で継続する。
5. 必要な作業を brief と証拠 ref にまとめ、明示的に新 lane を作成する。
6. 既存 Worker を新 lane に無断で付け替えない。

v0.x 方針に従い、旧 nested child のための復元互換層は作らない。新 run への引継ぎは、旧 provider 履歴の途中置換とは扱わない。

---

## 4. 共通検証と回帰ゲート

### test-policy

- EventBus、watch、oneshot、Notify、Barrier、runtime lifecycle wait で同期する。
- stall・cooldown は paused Tokio time／注入 clock で検証。
- 固定 sleep、固定 timeout による完了判定、実時間の速さを合否条件にしない。
- 「wake しない」は、配送処理終了 barrier 後の outbox・wake 記録で検証。
- transaction と ACK 境界の fault injection を使う。
- 低価値な重複テストは整理してよいが、必要な公開契約を失わない。

### cache gate

主要拡張先:
- `crates/runtime/tests/cache_preservation_e2e.rs`
- `crates/runtime/tests/claude_cache_e2e.rs`
- `crates/runtime/tests/thread_tools_cache.rs`
- `scripts/contract-test-filters.sh`

新 test binary は filter へ登録し、`scripts/check-cache-contracts.sh` で実行されることを確認する。

**input-derived mock cache と独立した wire-prefix assertion を両方維持**する。固定 cached-token 値で代用しない。

### 統合回帰

- lane を使わない通常会話・delegate。
- root Orchestrator 起動、モデル経由の child Orchestrator 拒否。
- Stop/Resume、restore、goal checks。
- lead/lane の ownership・admission・finish gate の独立。
- 質問の作成者・継承者・回答者の区別。
- 10+ lane、40 質問、20 回超の正常 wake。
- fork/rewind、thread 参照。
- family の pin/archive、retention、複数 GUI の merge。
- detached の通常 thread 扱い。
- 保存・起動・復元・ACK の障害境界。
