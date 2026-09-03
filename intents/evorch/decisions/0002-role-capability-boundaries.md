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
  - Orchestrator: delegate / read / grep / git_diff / compact / finish / web_fetch(network: OptIn) のみ。write / edit / apply_patch / arbitrary shell / git commit / web_search は持たせない。単一 URL 確認用途の fetch は許可するが、open-ended な検索は Librarian 専用とする
- 生成と独立レビューを別 context / 別 role にする（Planner → Reviewer、Worker → Reviewer）。
- web_fetch 開放の補足（2026-09-03 確定）: Orchestrator への fetch 提供は「小さな検証コストの delegation 往復」を削るためであり、`ContentOrigin::WebUntrusted` 型付けで untrusted 扱いを維持する。web_search は open-ended 調査の起点として Librarian 専用に据え置くことで capability discipline を保持する

## Consequences

- 自己正当化による品質低下を抑えられる。
- Orchestrator が「何でも自分でやる」問題を capability 制限で防げる。
- 各 Role に対応する sandbox policy との整合が必要。

## Related

- [features/orchestration](../features/orchestration/overview.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
- [identity/mission](../identity/mission.md)
- [ADR 0022: 親子限定ツリー addressing と can_delegate の Role capability 開放](0022-parent-child-tree-addressing-and-nested-delegation.md) — `can_delegate` の適用範囲拡大（boundary 設計自体は不変）
