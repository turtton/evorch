# Durable execution 語彙の補足（2026-09-18）

## 問い（durable 実行の語彙が既存概念と衝突しないか）

durable execution substrate の導入にあたり、`restore_delivery` /
`AgentContext::from_restored` / mission-recovery / goal state との語彙衝突を
確認する必要があった。結論として「既存類似機能を partial 置き換え」せず、
orchestration 層の公式記録として新たに durable task/run を導入した。

## 解決（v07-durable-execution-substrate / ADR 0026 / PR #119 で確定）

- **`AgentContext::from_restored`** = 1 agent の message/checkpoint context の再構成。
- **`restore_delivery`** = delivery 復元 + generation fencing。
- **durable continuation** = goal・論理 task・attempt・run generation・cursor・
  artifact・failure・resume/retry/cancel を接続する orchestration 層の記録。
  保存された状態から新 run を作成し、旧 generation の tool side effect は replay しない。
- durable task 状態語彙は shared orchestration vocabulary（Pending/Queued/Blocked/
  Retrying/Running/Completed/Failed/Cancelled）を再利用。goal 状態語彙とは別。

この区別により recover/resume の意味論が stack のどの層かで一意に決まる。

## 関連

- ADR: [0026-durable-execution-substrate](../decisions/0026-durable-execution-substrate.md)
- 状態遷移図: [durable-execution-states](../features/orchestration/durable-execution-states.md)
