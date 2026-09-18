# ADR 0026: Durable execution substrate（中断・再開・検証可能な長時間 agent 実行）（2026-09-18）

## Status

Accepted（PR #119 / issue #118 / execution unit v07-durable-execution-substrate で実装済み）

## Context

cache/queue 前提の agent 実行は in-memory で、`evorch` プロセスや pane の死活に
実行継続が依存していた。これにより (a) プロセス再起動・worker クラッシュ・ハングで
checkpoint より先の作業が全損する、(b) 高コスト provider/model に対して長時間実行の
予算超過・非収束の発見が遅れる、(c) 完了報告が検証なしの自己申告になる、(d) GUI が
実行継続を監視・介入する手段を持たない、という 4 つの問題があった。

v09 wave の体験記録（ADR 0025 の Evidence）は既に、session 再開・append-only 台帳・
並列 index 競合・provider 連敗時の停止判断が実運用の生存戦略であることを示していた。
本 ADR はそれらの負債を orchestration kernel の公式機能として引き上げた判断を確定させる。

## Decision

隔離 substrate を新設せず、既存の GoalSupervisor / OrchestratorEvent /
イベント replay / ReviewLoop / GUI event pump を拡張する。

1. **Durable task/run の状態記録。** SQLite v8 に durable task 状態（親子 linkage、
   cursor/progress/artifact/failure の persistence）を保持し、resume/retry/cancel、
   stale worker の heartbeat 検知、generation fence・terminal fence（遅延更新拒否）を
   提供する。`crates/storage/src/projection/durable.rs`、
   `crates/runtime/src/orchestration/ledger/durable.rs`。
2. **Budget & circuit breaker。** tool/token/checkpoint の accounting、
   50-call ごとの TaskCheckpoint、BudgetExhausted/NoProgress/ProviderUnavailable
   診断、hard breach 後の cooperative stop。**残予算 Warning は hard limit の 20% 残量
   閾値 crossing で一回限り発火**し、checkpoint → warning → exhaustion の順序を
   統合テストで固定する。`crates/runtime/src/budget_tracker.rs`。
3. **Evidence-first 検証ゲート。** reviewer 専用 typed `submit_review` チャネル、
   supervisor 向け typed-first transport（prose fallback 付き）、criterion ごとの
   evidence 検証、bounded review recovery。`crates/runtime/src/orchestration/supervisor/`。
4. **専用 GUI pane「Durable Tasks」。** session/live-agent row に混ぜず別 panel とし、
   task/goal/generation/run/attempt/checkpoint/状態/最終有効 artifact を一体表示する。
   元 worker 死亡後も有用であり続けるため。
5. **admission の generation 管理。** `TaskRetryScheduled` は `goal_id` を必須保持し、
   admission は直近 operational generation のみ監視する。admission 撤廃と再投入を
   idempotent に扱い、pending admission 中の cancel は登録直前の fence で止める。
   admission 失敗の generation は Failed 化・理由永続化し、別 run で retry 可能。

**状態語彙の共有。** durable task 状態（Pending/Queued/Blocked/Retrying/Running/
Completed/Failed/Cancelled）は shared orchestration vocabulary を再利用し、
storage/supervisor/replay/GUI が同一の typed representation を共有する。
goal レベルの状態語彙とは混同しない（goal と durable task の scope が異なるため）。

**restore_delivery / from_restored との関係。** `AgentContext::from_restored` は
1 agent の message/checkpoint context を再構成し、`restore_delivery` は delivery 復元と
generation fencing を担う。durable continuation はその上の orchestration record で、
goal・論理 task・attempt・run generation・cursor・artifact・failure・resume 操作を接続し、
旧 run の tool side effect を replay せず保存状態から新 run を生成する。

## Evidence（実装検証）

- runtime 792 テスト pass、修正差分の delta review で 5 blocker の動作固定テストを確認。
- 統合テスト: budget_checkpoint（9）、durable_admission（2）、provider_admission（5）、
  goal_state_machine（14）、durable_worker_boundary（5）、review_evidence_contract（8）他。
- hands-on QA で P0 シナリオ全 pass、GUI Durable Tasks の visual QA 3 状態 PASS。
- レビュー過程で admission 中 cancel race、retry 永久停止、replay ownership 曖昧など
  本番到達前に修正（PR #119 の repair ラウンド参照）。

## Consequences

- agent 実行は process/pane 利用可能を前提とせず、中断・再開が「壊れない」設計になる。
- 超過・非収束は BudgetExhausted/NoProgress/ProviderUnavailable で明示発生し、
  wrapped completion が検証ゲートを通る。
- GUI は既存 event pump を通じて lifecycle/artifact を別 pane で提示し、
  soggy session と混在しない。
- ADR 0025 が挙げた session 再開・台帳・provider 連敗対応の負債を公式機能として解消。
- process 外 provider の革命再起動・HIPAA 等の厳密な jurisdiction 担保は本 ADR 範囲外
  （"まだ" 実施しない）で、後続 unit の論点として残す。
