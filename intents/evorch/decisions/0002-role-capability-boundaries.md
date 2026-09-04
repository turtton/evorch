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

## 2026-09-03 確定: Direct→Orchestrator escalation の capability 影響

- escalation は capability boundary を変更するものではなく、run の lifecycle 遷移。
- escalation 時に workspace を引き継ぐ場合、排他的制御で二重変更リスクを防ぐ。
- memo スキーマ（`EscalationMemo`）は ADR 0001 に記載の通り、構造化された handoff 情報を保持する。

## 2026-09-05 確定: Orchestrator capability へ web_fetch(OptIn) を追加し、web_search は Librarian 専用に据え置く

- `Role::Librarian` を crates/agents/src/role.rs に追加 (allowed_tools: read / grep / web_search / web_fetch、NetworkAccess::Allowed、can_delegate=false)。
- Orchestrator: allowed_tools に web_fetch を追加 (17 tools)、NetworkAccess を Denied → OptIn。web_search は非公開のまま。Explorer / Worker / Reviewer は不変 (両 web tool を拒否)。
- model-visible surface: `standard_tool_specs()` (crates/runtime/src/agent_loop/tool_calls.rs) に web_search / web_fetch を登録し、`ExecutionPolicy::filter_tool_specs` (layer-1 role filter) が非対称を適用する。
- production registry: `ToolExecutor::with_web_tools()` (WebSearch::keyless_default / WebFetch::new) を `AgentRuntime::production` / `production_with_project` / isolated workspace の executor 構築に配線。NetworkGuard 初期化失敗は `RuntimeError::NetworkGuard` として fail-closed に伝播する。`with_production_sandbox` は sandbox-only の sibling entry のまま。
- tripwire: web_search_network_gate.rs / web_fetch_network_gate.rs を非対称公開へ更新 (Librarian=両方許可 / Orchestrator=fetch のみ OptIn / Explorer・Worker・Reviewer=拒否)。Orchestrator の web_fetch は agent loop が network 権限ツールへ `judge_web_network_access` (role / per-tool / session の 3 層 AND) を適用し、session 層 (`RunConfig::network_access`、既定 Denied = ADR 0008 fail-closed) が OptIn のとき EventBus の `ApprovalRequested` / `ApprovalResolved` による承認を経てのみ executor に到達する。session Denied では executor 到達前に拒否される (ToolStarted なし)。
- 承認は loop 側の `ApprovalGate` が 1 回だけ発行する (production executor の ApprovalPolicy は allow_all のまま = per-tool 層は AutoAllow で二重 ask なし)。承認待ちは cancel を尊重し、300 秒で TimedOut → error result として run は継続する。
- ContentOrigin::WebUntrusted は ToolExecutor が権限宣言から機械導出し fail-closed を維持 (回帰テスト追加)。
- sandbox 影響: Orchestrator の OptIn は explicit_opt_in=false では Unshared に解決され bwrap 挙動は不変。web tool は in-process reqwest で bwrap の外 (NetworkGuard が境界)。
- 未配線 (後続 slice): GUI からの `ApprovalResolved` 発行 UI と `network_access` 設定 surface (現状 GUI 起動の run は session Denied で fail-closed に拒否される)、delegate 系 meta-op への `network_access` 引数・親 run からの継承、Librarian の config フィールド / prompt baseline / `parse_role` / GUI 露出。
