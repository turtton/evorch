# v0.2: web ツールを role 別に公開する（Librarian=search+fetch / Orchestrator=fetch のみ OptIn）

## Goal

web_search / web_fetch 本体は v0.2 で実装済み（#43 / #45）だが、production の layer-1 gate（role capability filter）では現行の全 role で拒否されている（tripwire test で固定済み）。本 slice は role capability と production tool registry を更新し、Librarian に両方（NetworkAccess=Allowed）、Orchestrator に web_fetch のみ（NetworkAccess=OptIn）を公開する consumer 配線を行う。

## Why This Slice Exists Now

web tools 本体（#43 / #45）が landed し、v0.2 確定節の「Librarian の調査相棒」を production で実際に使える状態にするのが残課題。実装方針の root truth は `.intent-cli/issues/v02-web-tools-role-exposure/packet.yaml` の acceptance_criteria（権威）。

## Current Observed State

- ToolExecutor::with_standard_tools() は read / edit / grep / shell / git_diff のみ登録（crates/tools/src/executor.rs）
- production agent loop の model-visible spec は crates/runtime/src/agent_loop/tool_calls.rs の standard_tool_specs()、3 層 AND の layer-1 は crates/runtime/src/policy.rs の role filter
- runtime tripwire test（web_search_network_gate.rs / web_fetch_network_gate.rs）は現行の全 role で web ツール拒否を固定

## Accepted Baseline You May Assume

- web_search / web_fetch は crates/tools に実装済み（Exa keyless 既定 + Tavily fallback、readability chain + 5MB/50KB 切詰め）
- role capability は crates/agents/src/role.rs（Role::capabilities() / NetworkAccess::{Denied,OptIn,Allowed}）
- ContentOrigin 型付けは ToolExecutor の結果正規化層で fail-closed 機械導出（tool 自己申告不可）
- Rust 1.97 / edition 2024 + Tokio async runtime

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/agents/ crates/runtime/ crates/tools/
- Part: role capability と production tool registry の非対称公開配線（policy の model-visible filter と executor 登録、tripwire テストの非対称更新を含む）

## In Scope

- crates/agents/src/role.rs の Librarian / Orchestrator capability 更新（Librarian=search+fetch+NetworkAccess Allowed、Orchestrator=fetch のみ+NetworkAccess OptIn）
- crates/runtime/src/agent_loop/tool_calls.rs の standard_tool_specs() への web_search / web_fetch 登録（policy 層 role filter で非対称適用）
- crates/tools/src/executor.rs（または production composition root 等価経路）への web_search / web_fetch 登録
- tripwire テスト（web_search_network_gate.rs / web_fetch_network_gate.rs / capability_enforcement.rs）の非対称更新
- Orchestrator web_fetch の OptIn approval 経由 test、ContentOrigin fail-closed 回帰 test
- ADR 0002 への確定版追記（packet 指定 decision_title: 「Orchestrator capability へ web_fetch(OptIn) を追加し、web_search は Librarian 専用に据え置く」）

## Out Of Scope

- web_search / web_fetch 本体の変更（実装済みのまま）
- Explorer / Worker / Reviewer への web ツール公開
- skill loader / project rules / orchestrator loop / GUI 側の変更
- browser escalation、per-domain allowlist

## Standalone Child Issue Contract

本 issue は単独で読める。実装方針の root truth は `.intent-cli/issues/v02-web-tools-role-exposure/packet.yaml` の acceptance_criteria（権威）で、本 body はその転記と位置付けの説明に留まる。

## Acceptance Criteria

packet の acceptance_criteria が権威（以下は概要）:

1. crates/agents/src/role.rs で Librarian は web_search と web_fetch を許可、NetworkAccess=Allowed
2. Orchestrator は web_fetch のみ許可し NetworkAccess=OptIn（承認なしでは拒否）。web_search は Orchestrator から非公開のまま
3. crates/runtime/src/agent_loop/tool_calls.rs の standard_tool_specs() に web_search / web_fetch を登録し、policy 層の role filter で上記非対称が適用される
4. crates/tools/src/executor.rs の with_standard_tools()（または production composition root 側の等価経路）に web_search / web_fetch を登録し、本番 agent loop で到達可能にする
5. tripwire テスト web_search_network_gate.rs / web_fetch_network_gate.rs を非対称公開へ更新（Librarian=両方許可、Orchestrator=fetch のみ OptIn、Explorer/Worker/Reviewer では拒否）
6. Orchestrator の web_fetch 実行が session NetworkAccess=OptIn の approval を経由する runtime テスト
7. ContentOrigin::WebUntrusted が ToolExecutor で fail-closed に維持される回帰テスト
8. `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass

## Verification

- focused tests: tripwire 更新 2 件 + Orchestrator OptIn approval 経由 test + ContentOrigin 回帰
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）の blocker 0 / 承認済み

## Related Links

- intents/evorch/features/tools-sandbox/overview.md
- intents/evorch/decisions/0002-role-capability-boundaries.md（**ADR 更新が本 slice に必須**: 「Orchestrator capability へ web_fetch(OptIn) を追加し、web_search は Librarian 専用に据え置く」）
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/interviews/web-tools-v02.json
- dependency: `v02-web-search-tool`（#43、merged）、`v02-web-fetch-tool`（#45、merged）

## Knowledge Maintenance

- Intent placement: tools-sandbox overview へ role 公開方針と production wiring 更新ポイントを反映（lead が closeout 時に実施）
- ADR candidate: ADR 0002 へ確定版追記（packet 指定 decision_title、本 slice 同梱）
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（tools-sandbox overview）

## Guide Reachability (G645)

- guide_surface: tools-sandbox overview の tool 利用 surface（web_search / web_fetch）
- role: Librarian / Orchestrator
- target_surface: role capability（crates/agents/src/role.rs）と production tool registry

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
