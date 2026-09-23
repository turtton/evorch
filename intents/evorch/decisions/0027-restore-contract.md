# ADR 0027: 復元契約と実行状態の分離（2026-09-21、2026-09-23改訂）

## Status

Accepted（2026-09-21）。root履歴の明示的な権限更新と終端処理を2026-09-23に改訂。
改訂範囲は利用者が「OK。承認する」と最終承認した。
回答は `intents/evorch/interviews/harness-reliability-20260923.json` の
`final-approval` に記録している。
2026-09-24、利用者の「実行結果が取れなかったものはそういうエラーとして処理して
会話を継続させる」指示に基づき、第5節のchat継続契約を改訂した。

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
| renewable（team root限定） | `topology`（DynamicTeam）、`team`、`team_store`、`delegation_value` | root Orchestratorの履歴を、同一team IDに対する現在のTeamStore・topology・非空の委譲設定で継続する。下記の停止・claim条件を必須とする。 |
| blocking | `team_task`、non-rootのteam構造、childのmemory/finding、`learning_internal`、`workspace_branch` | 旧workerのlease、実行構造、学習処理、branchをこの入口で再開しない。常に拒否する。 |
| security/privacy | `skip_secret_redaction`などの配慮設定、およびhard blockerを含む組み合わせ | 保存済み情報から権限を拡張しない。ownershipやrenewable設定が同居してもhard blockerを緩めない。 |

### 3. root履歴の継続条件

- `continue_goal` / `delegate_chat` は記録のroot識別子、role、parentを検証する。
  停止済みrootの履歴を再利用するときは、元rootと既知の子孫が停止済みであることを
  確認する。provider admission待ちの子も対象とし、admissions→runsの同じlock順で
  登録との隙間を作らない。動作中rootへの通常のメッセージ送信は別経路である。
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
  team/claim・子孫停止・消費済み記録・非対応設定の確認は会話継続でも維持する。
  cleanup失敗で旧shell processが動作中なら継続を拒否する。新規snapshotは未確認結果と
  設定の拒否理由を独立に保持し、既存rootの `unresolved_tool_calls` 記録は現在権限で
  履歴だけを再利用する。escalation先の状態取得・thread間連携はこの変更に含めない。
- shell回収、最終保存、handle解放、workspace cleanupまたは引継ぎ準備を終えてから
  元runの終端状態と完了通知を公開する。同一IDで再開したrunに、旧runのdrain、
  snapshot書込、workspace inspection更新が触れない順序を守る。
- エスカレーションは質問継承とworkspace引継ぎ準備の後、元run終了通知→新root開始
  の既存順序を維持する。準備に失敗した場合は新rootを開始しない。
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

- `crates/runtime/tests/restore_expansion.rs`: 現在権限、再起動、team/claim、子孫停止、
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
