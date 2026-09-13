## Goal

外部 MCP tool registry と ADR 0008 準拠の permission scope、LSP diagnostics の
ToolExecutor/DiagnosticEvent 接続、provider 差分を吸収する canonical model capability、
CompactionEvent の Diagnostics/transcript 可視化を実装する（4要素で1ユニット）。

## Why This Slice Exists Now

v0.6 Bundle B の中核。tool 基盤（Tool trait / ToolExecutor）、権限境界（ADR 0008 の
fail-closed 経路）、EventBus の DiagnosticEvent/CompactionEvent、ModelCatalog は揃っているが、
MCP/LSP の接続と capability 正規化、compaction の可視化が未実装で、外部 tool 統合と
観測可能性が断片化している。

## Current Observed State

- MCP は crates/tools/src/search/mcp.rs の search 用単発 JSON-RPC transport のみ。
  汎用 client registry や server session 管理は存在しない。
- tool 権限検査・承認は crates/runtime/src/policy.rs、crates/agents/src/capability.rs、
  crates/sandbox/src/approval.rs（ApprovalGate::request、ToolEvent::ApprovalRequested/Resolved）
  が基盤。ADR 0008 の二層境界は crates/runtime/src/network.rs と crates/sandbox/src/composition.rs。
- DiagnosticEvent（source/severity/code/detail/run_id/thread_id）は schema と storage 経路が
  既存だが、LSP client / publishDiagnostics handler / compiler diagnostics の ToolResult 接続はない。
- v0.2 compaction は CompactionReason/CompactionEvent::Compacted（tokens before/after、reason、
  range）を event 発行するが、GUI の Diagnostics/transcript への可視化接続がない。
- ModelCatalog/CatalogCapabilities（crates/model）、ProviderCapabilities（crates/providers）、
  routing（crates/routing）があるが、provider 差分の canonical capability 正規化は未実装。

## Accepted Baseline You May Assume

- 統一 tool 境界: crates/tools/src/lib.rs の Tool trait / ToolExecutor。
- ADR 0008 の fail-closed 経路（NetworkAccess → SandboxNetworkMode、sandbox composition）。
- DiagnosticEvent / CompactionEvent の既存 schema と storage projection。
- ModelCatalog / ProviderCapabilities / routing の既存基盤。
- 2026-09-12 オペレータ確定スコープ: 4要素限定。追加 role capability（Planner/Librarian/
  Oracle/Multimodal Looker）と diagnostics v0.5 骨格は scope 外。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/tools/ crates/sandbox/ crates/runtime/ crates/event-bus/ crates/providers/ crates/model/ crates/routing/ crates/gui/`

Target part: MCP tool registry + permission scope / LSP diagnostics 接続 / canonical model
capability 正規化 / compaction の Diagnostics・transcript 可視化

## In Scope

- MCP server 定義から tools/list 相当で外部 tool を registry 登録し、ToolExecutor の
  name/schema/execute 経路で呼び出せるようにする
- MCP tool の network/filesystem/credential scope を RoleCapabilities・per-tool permission・
  session NetworkAccess の AND 条件で評価し、deny/未承認時は外部プロセス・通信を開始しない
- MCP tool の登録・拒否・実行失敗を ToolEvent/DiagnosticEvent として run_id/call_id に相関
  （credential や秘密値を detail に含めない）
- LSP client が workspace の compiler diagnostics を受信し、severity/file/range/message/code を
  canonical な tool outcome として返す
- LSP diagnostics を DiagnosticEvent として EventBus に発行し、run_id/thread_id を持ち、
  agent が次の turn で修正可能な actionable 内容を受け取る
- 異なる provider/model の capability 表現を canonical model capability に正規化し、
  routing と prompt assembly が同じ canonical 値を参照する
- capability 未知・欠落・provider 非対応は optimistic に有効化せず、unknown/unsupported として
  安全に degrade する
- CompactionEvent::Compacted の cache transition、CompactionReason、estimated_tokens_after を
  Diagnostics pane に表示し、automatic/manual/agent の理由を区別する
- 同じ compaction 情報が transcript に replay 後も表示され、storage 再生で欠落・重複しない

## Out Of Scope

- 追加 role capability（Planner/Librarian/Oracle/Multimodal Looker）
- diagnostics v0.5 骨格
- compaction engine 自体の変更（v0.2 の既存 policy/estimator/cut/summary を使う）
- EventBus schema の破壊的変更

## Standalone Child Issue Contract

evorch に (1) 外部 MCP tool registry（tools/list 取得 → ToolExecutor 経由で呼び出し可能、
ADR 0008 準拠の network/filesystem/credential scope を AND 評価、deny/未承認時は fail-closed、
登録/拒否/失敗を ToolEvent/DiagnosticEvent に相関・秘密値非含有）、(2) LSP diagnostics の
ToolExecutor/DiagnosticEvent 接続（severity/file/range/message/code の canonical outcome、
EventBus 発行・run 相関・agent actionable）、(3) canonical model capability 正規化（routing と
prompt assembly が同一 canonical 値参照、unknown/unsupported は安全に degrade）、
(4) CompactionEvent の Diagnostics pane/transcript 可視化（理由区別、replay 後も欠落・重複なし）
を実装し、各要素の回帰テストを添えて PR として提出する。

## Acceptance Criteria

- MCP server 定義から tools/list 相当で取得した外部 tool が registry に登録され、
  ToolExecutor から通常の runtime tool として name/schema/execute 経路で呼び出せる（cargo test）
- MCP tool の network/filesystem/credential scope が RoleCapabilities・per-tool permission・
  session NetworkAccess の AND 条件で評価され、deny または未承認時には外部プロセス/通信を
  開始しない（cargo test）
- MCP tool の登録・拒否・実行失敗が ToolEvent または DiagnosticEvent として run_id/call_id に
  相関し、credential や秘密値を detail に含めない（cargo test）
- LSP client が workspace の compiler diagnostics を受信し、severity、file、range、message、
  code を canonical な tool outcome として返す（cargo test）
- LSP diagnostics が DiagnosticEvent として EventBus に発行され、対応する run_id/thread_id を
  持ち、agent が次の turn で修正可能な actionable 内容を受け取る（runtime 統合テスト）
- 異なる provider/model の capability 表現が canonical model capability に正規化され、routing と
  prompt assembly が同じ canonical 値を参照する（model/routing の cargo test）
- capability 未知・欠落・provider 非対応の場合は optimistic に有効化せず、明示的な
  unknown/unsupported として安全に degrade する（cargo test）
- CompactionEvent::Compacted の cache transition、CompactionReason、estimated_tokens_after が
  Diagnostics pane に表示され、automatic/manual/agent の理由を区別できる（GUI headless test
  または observable label）
- 同じ compaction 情報が transcript に replay 後も表示され、storage からの再生で欠落・重複
  しない（crates/gui/tests および storage/runtime の cargo test）

## Verification

- `cargo test --workspace` 全緑（上記各観点の回帰テストを含む）
- `git diff --check`

## Related Links

- ADR 0008: intents/evorch/decisions/0008-threat-model-phased-adoption.md
- ADR 0002: intents/evorch/decisions/0002-role-capability-boundaries.md
- ADR 0004: intents/evorch/decisions/0004-provider-routing-separation.md
- ADR 0013: intents/evorch/decisions/0013-model-catalog.md
- ADR 0020: intents/evorch/decisions/0020-canonical-message-normalization.md
- intents/evorch/technology/mvp-roadmap.md（Bundle B）

## Knowledge Maintenance

- Intent placement: tools-sandbox overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

- guide surface: `intents/evorch/technology/mvp-roadmap.md`（role: implementation）
- target surface: Bundle B の MCP permission scope、LSP diagnostics feedback、model capability
  normalization、Diagnostics / transcript の compaction 表示

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
