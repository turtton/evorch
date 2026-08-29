# ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する

## Status

Accepted

## Context

1 つの LLM に調べる・設計する・実装する・レビューする・修正するを全部やらせると、自分が一度選んだ案を正当化する方向へ寄りやすい。従来の「あなたは調査だけしてください」という prompt による制御は弱い。

## Decision

Role を personality ではなく capability boundary とする。

- Orchestrator / Explorer / Librarian / Oracle / Planner / Reviewer / Worker / Multimodal を分離する。
- runtime レベルで tool 権限を制限する。
  - Explorer: read / search allowed、write / edit / delegate denied、network optional
  - Librarian: read / search / network allowed、write / edit / delegate denied
  - Worker: workspace read-write、network denied by default
  - Orchestrator: delegate / read / grep / git_diff / compact / finish 等のみ、write / edit / apply_patch / arbitrary shell / git commit は持たせない
- 生成と独立レビューを別 context / 別 role にする（Planner → Reviewer、Worker → Reviewer）。

## Consequences

- 自己正当化による品質低下を抑えられる。
- Orchestrator が「何でも自分でやる」問題を capability 制限で防げる。
- 各 Role に対応する sandbox policy との整合が必要。

## Related

- [features/orchestration](../features/orchestration/overview.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
- [identity/mission](../identity/mission.md)
