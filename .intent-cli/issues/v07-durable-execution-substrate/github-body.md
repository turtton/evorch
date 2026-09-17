## Goal

長時間・多 agent 実行を「中断・再開・検証」できる durable execution substrate を、既存の GoalSupervisor / OrchestratorEvent / event replay / ReviewLoop の拡張として実装する。

## Why This Slice Exists Now

2026-09-17 の ulw 7項目修正 wave (a7dc0ab..13d8a04) を OpenCode ハーネスで実行した際、model fallback 未検証 (worker 二度死亡)、400-call cap (test sweep 強制終了)、Oracle stream 切断 (長文レポート二度喪失)、orchestrator による手動 notepad 管理が観測された。evorch は orchestration / bounded review / event replay / diagnostics 土台を持つが durable execution substrate がない。

## Current Observed State

- `GoalSupervisor` は goal の lifecycle を管理するが、crash/resume の state machine はない
- `OrchestratorEvent::GoalCreated` に thread_id/root_run_id があるが、task_id ごとの永続化・再開機構はない
- ReviewLoop は bounded だが、verdict は prose parse 依存で evidence 紐付けがない
- budget/circuit breaker はなく、runaway agent は hard cap でのみ止まる

## Accepted Baseline You May Assume

- 既存の `GoalSupervisor`, `OrchestratorEvent`, event replay, `ContinuationDecision`, `DiagnosticEvent` (code=CacheRegression 等の typed code パターン), MCP registry, skill load, compaction
- intent: features/orchestration/overview.md, features/agent-runtime-kernel/overview.md

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime`, `crates/gui`, `crates/event-bus`

Target part: GoalSupervisor・OrchestratorEvent・event replay・ReviewLoop の拡張として、durable task continuation / budget & circuit breaker / evidence-first verification gate を実装する

## In Scope

1. **Durable Task/Run Continuation**: task_id 永続化 (parent/child, 入力, 進捗, 成果物, 失敗理由, 再開地点), `resume(task_id)`, idempotent retry, cancel, stale worker 検出, GUI run status 表示
2. **Budget & Circuit Breaker**: tool-call/時間/token/再読/無進捗 round 予算化, 50-call checkpoint, 反復検知, spawn 前 provider/model 疎通確認, DiagnosticEvent typed code 化
3. **Evidence-first Verification Gate**: 既存 ReviewLoop 維持, criterion への RED/GREEN/exit status/SHA/diff/artifact path 紐付け, typed tool result による verdict/evidence 返却

## Out Of Scope

- P1 (Durable Context Notebook, Role/Capability Routing, Versioned Skill Contract)
- P2 (Code Intelligence Provider)
- OpenCode team-mode 完全模倣, skill 自動 install, 汎用 MCP marketplace, role 細分化競争, intent-cli コア再実装

## Standalone Child Issue Contract

この issue は、evorch の orchestration 基盤に「中断・再開・検証可能な長時間 agent 実行」のための durable execution substrate を導入する。child PR は、task_id 永続化 + resume/cancel/retry state machine、tool-call/time/token budget + circuit breaker、evidence 紐付き ReviewLoop verdict の3機能を、既存 GoalSupervisor/OrchestratorEvent/event replay/ReviewLoop の拡張として実装し、GUI に run status を表示し、既存テストの回帰なく CI をグリーンにする。

## Acceptance Criteria

- task_id 指定で中断後の goal を resume でき、成果物・失敗理由・再開地点が永続化されている
- tool-call 数・時間・token 等の予算超過時に DiagnosticEvent (BudgetExhausted/NoProgress/ProviderUnavailable) が発行され、checkpoint が 50-call ごとに発火する
- spawn 前に provider/model が解決・疎通確認され、未検証 fallback で worker が起動しない
- ReviewLoop の verdict が typed tool result (criterion + evidence 紐付け) で返される
- 既存 gui/providers/event-bus/runtime のテストが回帰しない

## Verification

- 各 P0 機能の単体テスト + goal 統合テスト (resume after crash, budget checkpoint firing, evidence-linked gate verdict)
- 既存 gui 766 + providers/event-bus/runtime 回帰なし
- CI 全ジョブ green (ci / nix-build / browser-e2e / offscreen-gate)
- `git diff --check` clean

## Related Links

- intents/evorch/features/orchestration/overview.md
- intents/evorch/features/agent-runtime-kernel/overview.md
- 2026-09-17 ulw session commits: a7dc0ab..13d8a04

## Knowledge Maintenance

- Intent placement: intents/evorch/features/orchestration/overview.md (primary), agent-runtime-kernel/overview.md (supporting)
- ADR candidate: 「Durable execution substrate as GoalSupervisor extension」— P0 開始時にドラフト
- Diagram candidate: goal/task lifecycle state machine (resume/cancel/retry 遷移)
- Docs update: docs/ に operator 向け「長時間実行の中断・再開」ガイド
- Closeout writeback expected: yes (clarifications/ へ event replay/binding パターンと予算設計)

## Guide Reachability (G645)

`no_role_facing_surface: false` — GUI の run status 表示 (queued/running/blocked/retrying/completed) は implementation role の surface。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
