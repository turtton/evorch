# ADR 0027: 復元契約と実行状態の分離（2026-09-21、2026-09-23改訂）

## Status

Accepted（2026-09-21）。root履歴の明示的な権限更新と終端処理を2026-09-23に改訂。
改訂範囲は利用者が「OK。承認する」と最終承認した。
回答は `intents/evorch/interviews/harness-reliability-20260923.json` の
`final-approval` に記録している。
2026-09-24、利用者の「実行結果が取れなかったものはそういうエラーとして処理して
会話を継続させる」指示に基づき、第5節のchat継続契約を改訂した。
2026-10-25、利用者の明示決定により、Done/Error の root も子孫を動かしたまま
継続でき、同じ thread に複数の live root を許す契約へ改訂した（末尾の追補）。

## Context

run-29ではGUIが現在のownership permitを再発行するにもかかわらず、保存済みの
`ownership` を復元不可理由として `continue_goal` が履歴の継続を拒否した。
初版はownershipをrenewable authorityと分類し、履歴と実行権限の区別を定義した。

初版はDynamicTeam、memory、findingなどを全入口で一律に拒否した。そのため、
現在の呼出元が権限・保存先を明示できるrootでも履歴を継続できなかった。
今回、ADR 0026の既存run/task基盤を維持したまま、停止したrootの会話履歴を
現在の設定で再利用する契約を追加する。旧worker、process、lease、workspace branch
の再開はこの改訂に含めない。

## Decision

復元可否はrun phaseやdurable task状態とは独立した、設定と復元入口の契約とする。

### 1. 判定の一元化

`restore.rs` の書込側と復元入口は次の定数・helperを共有し、理由文字列を
入口ごとに複写しない。

- `OWNERSHIP_ONLY_UNRESTORABLE_REASON` / `renewable_ownership_only()`
- `ROOT_CONTEXT_RENEWAL_REQUIRED_REASON` / `renewable_root_context()`
- `TEAM_RENEWAL_REQUIRED_REASON` / `renewable_team_root()`

新しい2つのマーカーもdiskだけで復元可能という意味ではない。呼出元が現在の
設定を供給し、以下の条件を検証できる入口に限り、履歴の再利用を許す。

### 2. 設定フィールドの分類

| class | フィールド・条件 | 契約 |
|---|---|---|
| renewable | `ownership` | 古いpermitを復元せず、呼出元の現在の権限を検証して付与する。既存のownership-only判定を維持する。 |
| renewable（root限定） | `memory`、`finding_store` | Single/DynamicTeamのrootだけを対象とし、現在の設定を使う。旧MemoryBoundaryや保存先を開かない。履歴中のlessonは過去の参考情報に留める。 |
| renewable（team root限定） | `topology`（DynamicTeam）、`team`、`team_store`、`delegation_value` | root Orchestratorの履歴を、同一team IDに対する現在のTeamStore・topology・非空の委譲設定で継続する。下記の実行競合・claim条件を必須とする。 |
| blocking | `team_task`、non-rootのteam構造、childのmemory/finding、`learning_internal`、`workspace_branch` | 旧workerのlease、実行構造、学習処理、branchをこの入口で再開しない。常に拒否する。 |
| security/privacy | `skip_secret_redaction`などの配慮設定、およびhard blockerを含む組み合わせ | 保存済み情報から権限を拡張しない。ownershipやrenewable設定が同居してもhard blockerを緩めない。 |

### 3. root履歴の継続条件

- `continue_goal` / `delegate_chat` は記録のroot識別子、role、parentを検証する。
  Stopped/Done/Error のroot履歴は、子孫が動作中でも再利用できる。同じroot IDの
  live incarnationまたはprovider admission待ちは拒否する。Checkpoint記録は、
  元rootと既知の子孫（provider admission待ちを含む）が動作中なら引き続き拒否する。
  admissions→runsの同じlock順で登録との隙間を作らない。
  動作中rootへの通常のメッセージ送信は別経路である。
- 同じthreadに異なるIDのrootが複数動作することを許す。会話単位の排他制限は加えず、
  同じroot IDの競合だけを防ぐ（Checkpointの子孫条件は上記のとおり）。
- DynamicTeamの記録にはteam IDとcoordinator run IDだけを保存し、DB path、writer、
  permit、lease、semaphoreを含めない。記録だけを根拠にTeamStoreを開かない。
- 現在の呼出元が同一team IDのTeamStore、DynamicTeam topology、非空の委譲設定を
  明示する。現在のboardを検証し、claimed taskがあれば拒否する。期限切れclaimも
  自動的に安全とはみなさず、旧ownerの副作用を照合してから既存の管理手段で処理する。
- 既存のteam初期化経路からboardのrevision/generationを読み込む。完了taskを
  再実行せず、ready taskも復元だけでは実行しない。stale writerのrevision fenceを維持する。
- ownership、network access、workspace設定、model、skills、worker limitは現在の
  呼出元から供給する。旧RunEntryやdescriptorから実行権限を暗黙に引き継がない。
- `continue_goal` はプロセス再起動後も保存済みrootを検証し、同じgoal run IDで継続できる。
  `delegate_chat` は新しいrun IDを使う。必要な質問・回答は明示的なconsumer linkを
  永続化してから引き継ぎ、新しいrootの開始より先に配送先を確定する。
  継承質問のIDと元requester provenanceは保持する。既存IDへの回答は現在の
  recipient / ownershipを検証し、正当な継承先threadだけを許可する。

### 4. 入口ごとの権限源

| 復元入口 | 権限の供給源 | renewable履歴 | blocking / security |
|---|---|---|---|
| `continue_goal` | 呼出元の現在の `RunConfig` | 上記条件を満たす場合のみ許可 | 拒否 |
| `delegate_chat` | 同上 | 上記条件を満たす場合のみ許可 | 拒否 |
| `restore_and_deliver` | disk descriptorのみ | **不可** | 拒否 |

`restore_and_deliver` は現在のteam/storage authorityを供給できないため、
`restorable == false` の記録を理由によらず拒否する。単なるメッセージ送信に
新しい権限を付与する効果を持たせない。拒否は最後のcheckpointを消費しない。
既存の同期的な `snapshot_consumed` 判定を維持する。このマーカーは消費済み履歴の
再利用防止というbookkeepingであり、設定フィールドのclassには含めない。

### 5. 未確認の副作用と終了境界

- tool batchをdispatchする前に、protocol上完結した履歴prefixとcall ID/nameを
  保存する。引数本体はこの診断metadataに保存しない。run storeが設定されている場合、
  intent保存の失敗後はtoolをdispatchしない。
- 結果が揃う前のcrash/cancelや、終了結果を未確認のshell jobは
  `interrupted_tool_calls` として残す。2026-09-24の利用者指示により、
  `continue_goal` / `delegate_chat` は未取得結果を `ToolExecutionOutcomeUnknown`
  の通知として履歴末尾に追加し、現在の権限で会話を継続する。結果の成功・無副作用は
  仮定せず、旧tool/jobを再実行しない。tool call本体を保存していない不完全batchには
  引数や孤立したToolResultを捏造せず、call ID/nameを含む通知を使う。
  既存履歴・cancelled結果・cache prefixは書き換えない。
- `restore_and_deliver` は副作用不明の記録を引き続き拒否する。現在の権限・root識別・
  team/claim・第3節の実行競合条件・消費済み記録・非対応設定の確認は会話継続でも維持する。
  cleanup失敗で旧shell processが動作中なら継続を拒否する。新規snapshotは未確認結果と
  設定の拒否理由を独立に保持し、既存rootの `unresolved_tool_calls` 記録は現在権限で
  履歴だけを再利用する。escalation先の状態取得・thread間連携はこの変更に含めない。
- shell回収、最終保存、handle解放、workspace cleanupまたは引継ぎ準備を終えてから
  元runの終端状態と完了通知を公開する。同一IDで再開したrunに、旧runのdrain、
  snapshot書込、workspace inspection更新が触れない順序を守る。
- エスカレーションは質問継承とworkspace引継ぎ準備の後、元run終了通知→新root開始
  の既存順序を維持する。準備に失敗した場合は新rootを開始しない。
  継承未回答質問は新Orchestrator threadに表示し、開始時に既存質問への回答を
  案内する。元threadには引き継ぎ済みの旨と移動先を表示し、新IDで再質問しない。
  storage再起動復元と複数continuationでも質問と現在の担当の追跡を維持する。
- 未確認shell結果のhandleは、回収と副作用マーカーの保存を確認してから解放する。
  回収や必要な保存を確認できなければhandle/workspaceを保持して失敗を報告する。
  未確認shell副作用がないrunのsnapshot保存失敗は診断に残し、既存の非致命扱いを保つ。

未確認の副作用を自動承認する経路は設けない。ただし、不明な結果をエラーとして
引き継ぐことは副作用の承認ではなく、会話の継続を妨げない。結果に依存する追加作業では
現状確認を促す。全ての追加操作を意味解析で制限する新たな機構は導入しない。

### 6. 診断と将来の変更

`restore_diagnostics` は最後の保存時刻・phase、履歴量、compaction checkpoint数、
復元可能範囲、拒否理由、durable task/team識別子、未確認callを読み取り専用で返す。
条件付きで履歴再利用が可能という表示は、現在の権限やclaim照合が確認済みという
意味ではない。保存なしと破損も区別する。

`RunConfig` に新しい復元不可フィールドを加える場合は、merge前に本ADRのclassと
入口ごとの挙動を定義し、テストで固定する。設定追加だけを根拠に自動的な緩和や
一律の禁止を増やさない。

## Evidence（実装検証）

- `crates/runtime/tests/restore_expansion.rs`: 現在権限、再起動、team/claim、
  admission、memory/finding境界。
- `crates/runtime/tests/restore_tool_intent.rs`: dispatch前保存、未完了tool、取消、保存失敗。
- `crates/runtime/tests/shell_jobs_integration.rs`: process回収、同一ID即時継続、
  35回取消後もhandle枯渇しないこと、未確認副作用の保持。
- `crates/runtime/tests/restore.rs`、`escalation_handoff.rs`、`escalation_questions.rs`:
  既存の非致命保存失敗、終端通知順序、質問継承失敗時の起動抑止。
- GUIのgoal復元テスト: 現在のproject/threadの権限、拒否理由、別projectの拒否。
- 実行結果・再現手順は `docs/harness-validation.md` と `scripts/check-harness.sh`。

## Consequences

run/taskの実行状態と、履歴の復元可否を分離したまま、現在の呼出元が権限を供給する
rootの継続範囲を広げる。新しい実行engineや旧workerの再開機構は導入しない。
blocking/securityの複合は引き続き拒否し、履歴から権限や未確認の副作用を復活させない。
関連する実行状態の契約は `features/orchestration/durable-execution-states.md` を参照。

## Addendum（2026-10-14、ADR 0031 ③ isolated workspace の削除）

[ADR 0031](0031-remove-isolated-workspaces.md) で isolated workspace を削除し、`RunConfig` と
`RunRestoreDescriptor` から `workspace_mode` / `workspace_branch` がなくなった。上の表の
blocking にある `workspace_branch` は、拒否する対象そのものが存在しなくなったので、
現在は該当しない。復元記述子は worktree を扱わない。

## Addendum（2026-10-22、親による子の継続と利用者の停止）

過去スレッドの調査で、親の ownership を継承した子のsnapshotがすべてownership-onlyとなり、
第4節のとおり `restore_and_deliver` が親から終了済みの子へのメッセージを常に拒否していた
ことを確認した。親は文脈を持つ子を継続できず、新しい子の作り直しを繰り返していた。
質問回答も Done の子だけを継続し、失敗した子は親が使える復旧手段のない
「explicit recovery」で拒否していた。利用者の承認（2026-10-22）により次のとおり改める。

- 送信者が直接の親であるとき、`restore_and_deliver` は親の現在のpermitで
  ownership-only記録を更新して子を継続する。質問回答による継続も同じ判定を使う。
  親以外の送信者（子から親への送信を含む）はdisk descriptorだけで判定する従来の契約を保つ。
- 親による子の継続では、第5節の `ToolExecutionOutcomeUnknown` 通知で未取得結果を伝えて
  継続する。旧tool/jobは再実行しない。
- 質問回答は Done に加えて、失敗（Error）で終わった子も継続する。
- 利用者が停止した run は、その thread の root を利用者が再開するまで、親のメッセージや
  回答で継続しない（`RunRestoreFailure::StoppedByOperator`）。root の再開は停止した木全体の
  継続を許す。取り消し（cancel）された run は継続しない（`RunRestoreFailure::Cancelled`）。
- blocking/securityの拒否理由、消費済みsnapshot、識別子の検証は変えない。

検証: `crates/runtime/tests/subagent_continuation.rs`、`subagent_question_resume.rs`。

## Addendum（2026-10-22、終了した親への子の報告）

子の完了通知は、親が終了・停止していると破棄されていた。親より長く動いた子の結果は
親に届かなかった。利用者の承認（2026-10-22）により、無限ループの防止を条件に親を起こす。

- 終了・停止した親への子の完了通知と子からのメッセージは破棄せず保留する
  （`DeliveryDisposition::Held`）。取り消された親への通知だけは破棄する。
- 保留した親は起こす。root は `LifecycleEvent::RootWakeRequested` でホストに依頼し、
  ホストが現在の権限で `wake_chat` により再開する。非rootの親は、生存している自分の親の
  現在のpermitで継続する。生存していなければ、親が再開されるまで保留する。
  保留分は run の再登録時に受信箱へ移す。
- disk だけで復元できる親への子のメッセージは、従来どおり disk から復元して配送する。
- 利用者が停止した親は起こさない。利用者が root を再開した時点で保留分を配送する。
- 利用者の入力なしに起こせる回数は会話ごとに `AUTO_WAKE_LIMIT`（20）回までとする。
  超えたら `LifecycleEvent::AutoContinuationPaused` を一度だけ出し、以後の保留分は
  利用者の次の入力まで残す。利用者のメッセージと `/continue` が上限を戻す。
  ホストが自動で行う再開（`wake_chat`）は利用者の入力として扱わない。

検証: `crates/runtime/tests/held_reports.rs`、`messaging_completion.rs`、
`crates/gui/tests/headless_conversation/thread_resume_status.rs`。

## Addendum（2026-10-25、子孫を残した Done/Error root の継続）

利用者は「完了またはエラーで終わった親を、子がまだ動いているときに再開してよい」、
「同じ会話の中で複数の root run が同時に動くことを許してよい」と明示決定した。
operator Stopped の例外だけでは、Done/Error の親の手動継続や、別の子が動いている
間の保留報告による自動wakeを拒否していたため、第3節をこの決定に合わせて改訂する。

- Stopped/Done/Error 記録は、生存中またはadmission待ちの子・孫がいても継続できる。
  同じroot IDの Pending/Running/Waiting incarnationと未完了admissionは引き続き拒否する。
- `continue_goal` / `resume_chat` / `wake_chat` は同じroot IDで再登録する。
  生きている子のparent linkは変更せず、その後の報告は新しい親のmailboxへ届く。
  `list_agents`、`wait`、`run_output` も既存の親子識別を使い、旧delegate/toolを再実行せず
  子の状態確認・待機・結果回収を行える。
- 保留メッセージは再登録時に、登録と同じruns lock下でキューから取り除きmailboxへ移す。
  子の終端報告の一度だけのrelay、operator Stop中の自動wake抑止、
  `AUTO_WAKE_LIMIT` は変更しない。
- 自動wakeの受理時にもoperator Stopを検証し、既に発行されたwake要求が後発のStopを
  越えて再登録しない。Stopとchat復元は同じrestore gateで、Stop確認から同期登録または
  admission予約までを直列化する。予約後のStopはadmissions→runsのfenceで登録を抑止する。
  admission待ちのStopも明示resumeまで保持する。拒否・admission取消時はsnapshotと
  保留報告を残し、利用者の `resume_chat` / `continue_goal` だけが停止を解除できる。
- `delegate_chat` は従来どおり新しいroot IDで履歴を再利用する。同じthreadの他のlive rootは
  拒否理由にしない。旧rootの子を新rootへ付け替える変更ではなく、旧parent linkと
  旧root宛ての報告を維持する。
- Checkpoint は終端phaseではなく、tool dispatch前や実行途中にも保存される。
  crash等による中断記録の安全条件をこの依頼で拡張せず、既存の子孫拒否を維持する。
- 元rootのshell cleanup、ownership・root identity・設定検証、消費済みsnapshotの拒否、
  未確認結果の `ToolExecutionOutcomeUnknown` 通知と旧tool/jobの非再実行を維持する。
  拒否はsnapshotを消費しない。DynamicTeamも同一teamの現在権限とclaim照合を維持し、
  生きているworkerのclaimed task（期限切れを含む）があれば引き続き拒否する。

検証: `crates/runtime/src/runtime/chat_restore/renewal.rs` のphase別競合テスト、
`crates/runtime/tests/held_reports.rs` のDone/Error再開・子の結果回収・保留分の一度だけの配送。
関連feature: [durable execution states](../features/orchestration/durable-execution-states.md)。
