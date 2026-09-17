# v07-durable-execution-substrate Implementation Packet

## Goal

長時間・多 agent 実行を「中断・再開・検証」できる durable execution substrate を、既存の GoalSupervisor / OrchestratorEvent / event replay / ReviewLoop の拡張として実装する。新しい並列基盤は作らない。

## Why

2026-09-17 の ulw 7項目修正 wave (commits a7dc0ab..13d8a04) を OpenCode ハーネスで実行した際、以下が観測された:

- model fallback の未検証 (`deepseek-v4-flash` unknown provider) で worker が二度死んだ
- test sweep agent が exhaustive read で 400 tool-call cap に達し強制終了された
- Oracle の長文レポートが stream 切断で二度失われた
- orchestrator が `/tmp` の notepad に契約・進捗・証跡を手動管理していた

evorch は orchestration / bounded review / event replay / diagnostics / MCP / skill load / compaction の土台を持つが、**durable execution substrate** がない。これは Oracle (2 rounds) と S1 worker testimony で合意された P0 ギャップ。

## Scope

P0 の3機能:

1. **Durable Task/Run Continuation**: task_id ごとに parent/child、入力、進捗、最終成果物、失敗理由、再開地点を永続化。`resume(task_id)`、idempotent retry、cancel、stale worker 検出を一つの state machine に。既存 `GoalSupervisor` + `OrchestratorEvent::GoalCreated` + event replay + `ContinuationDecision` を拡張。GUI に queued/running/blocked/retrying/completed と「最後の有効な成果物」を表示。
2. **Budget & Circuit Breaker**: agent ごとに tool-call数、経過時間、token、同一ファイル再読、無進捗 round を予算化。50-call checkpoint、反復検知、残予算警告を先に発火。provider/model は spawn 前に解決・疎通確認し fallback 候補も同時検証。発火先は既存 `DiagnosticEvent` で `BudgetExhausted`/`NoProgress`/`ProviderUnavailable` を typed code に。
3. **Evidence-first Verification Gate**: 既存 bounded `ReviewLoop` (orchestration/review.rs:83) を維持しつつ、各 criterion に RED証跡/GREENコマンド/exit status/対象 SHA/diff/artifact path を紐付ける。reviewer の prose parse を減らし typed tool result として verdict/evidence を返させる。

## Out of scope

- P1 (Durable Context Notebook、Role/Capability Routing、Versioned Skill Contract) — 別 packet 候補
- P2 (Code Intelligence Provider — MCP registry への CodeGraph 相当 optional provider)
- OpenCode team-mode 完全模倣、skill 自動 install、汎用 MCP marketplace、role 細分化競争、コアへの intent-cli 再実装 (Oracle「入れない方が良い」に合意)

## Verification

- 各 P0 機能の単体テスト + goal 統合テスト (resume after crash, budget checkpoint firing, evidence-linked gate verdict)
- 既存 gui 766 + providers/event-bus/runtime 回帰なし
- CI 全ジョブ green (ci / nix-build / browser-e2e / offscreen-gate)

## Knowledge Maintenance (G461, optional)

- Intent placement: intents/evorch/intent-tree/ の orchestration ノードに接続。新規 intent node 不要。
- ADR candidate: 「Durable execution substrate as GoalSupervisor extension」— P0 開始時にドラフト。
- Diagram candidate: goal/task lifecycle state machine (resume/cancel/retry 遷移)。
- Docs update: docs/ に operator 向け「長時間実行の中断・再開」ガイド。
- Closeout learning: event replay/binding パターンを clarifications/ に書き戻す (`write_back_required: true`)。

- Guide reachability (G645): `no_role_facing_surface: false` — GUI の run status 表示は implementation role の surface。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
