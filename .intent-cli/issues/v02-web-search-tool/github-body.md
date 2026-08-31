## Goal

`web_search` tool を builtin Tool 層（統一 Tool trait）に実装し、API key 設定ゼロで動く検索を Librarian に提供する。既定は Exa keyless（`https://mcp.exa.ai/mcp` への JSON-RPC HTTP POST）、失敗時（429 / 5xx / timeout）に Tavily keyless（`X-Tavily-Access-Mode: keyless`）へ一次 fallback。実行は bwrap 外 main process、3 層 AND（role capability ∧ per-tool permission ∧ session NetworkAccess mode）で gate し、結果は `ContentOrigin::WebUntrusted` 型付け + 制御マーカーエスケープを経て model に渡る。

## Why This Slice Exists Now

v0.2 roadmap の「Librarian / Oracle 追加」と ADR 0008 v0.2 項目「ContentOrigin 実装」の対になる slice。ADR 0002 で Librarian は network allowed の調査 role と定義されているが、`crates/tools` には network を要求する tool が 1 つもなく、role capability が宣言だけのまま。grill session `web-tools-v02`（10/10 accepted）で実装形態が全確定した（q01: 2 本分離 / q02: builtin + keyless MCP transport / q03: Exa keyless 既定 + Tavily 一次 fallback / q06: ToolExecutor 機械導出 / q07: 3 層 AND + main process / q10: 既存 event + metadata detail）。pi#1432 の教訓（provider-native search で cost / fallback 経路が不明）から、provider 経路と fallback を metadata で観測可能にすることを契約に含める。

## Current Observed State

- `crates/tools/` の標準ツールは read / edit / grep / shell / git_diff の 5 本のみ。network を要求する tool は存在しない
- `ToolResult`（`crates/tools/src/result.rs:3-15`）は `#[non_exhaustive]` で「v0.2 で出力の由来を表す `ContentOrigin` フィールドを追加する予定（ADR 0008）」と doc に明記されたまま未実装
- `ToolEvent::ToolCompleted`（`crates/event-bus/src/event.rs:212-220`）は `tool_name` / `call_id` / `is_error` のみで、tool-specific metadata を運ぶ detail を持たない
- `ToolExecutor`（`crates/tools/src/executor.rs:42`）は jsonschema 引数検証・`ToolStarted` / `ToolCompleted` emit・結果正規化層での制御マーカーエスケープ（`sanitize::escape_control_markers`）を既に実装しており、web tool はこの既存契約に乗る
- `NetworkAccess::{Denied, OptIn, Allowed}`（`crates/agents/src/capability.rs:10-18`）は v0.1.1 で `SandboxNetworkMode` への fail-closed 写像済み（`crates/runtime/src/network.rs`）だが、tool 層の network 要求 tool 向け判定経路は未接続
- provider client（`crates/providers`）は main process + per-call auth injection の既存パターン。web tool の実行位置（bwrap 外 main process）はこの延長
- `reqwest` 0.12（json / stream / rustls-tls）と `wiremock` 0.6 は workspace dependencies に既存

## Accepted Baseline You May Assume

- ADR 0002: Librarian は read / search / network allowed、write / edit / delegate denied。role capability は runtime レベルで強制する
- ADR 0008: credential を agent プロセス・子プロセス・環境変数へ渡さない（本 slice では provider credential を main process 環境変数のみで消費し sandbox 内非露出とする延長）。ContentOrigin 型付けは v0.2 実装項目。制御マーカーエスケープは ToolExecutor 結果正規化のみ、ディスク書き込みはバイト一致
- ADR 0021: bwrap の netns は all-or-nothing。web tool は bwrap 外 main process 実行のため sandbox network mode の変更を不要とする
- grill web-tools-v02 確定（`intents/evorch/interviews/web-tools-v02.json`）: 2 本分離 / builtin + keyless MCP transport / Exa keyless 既定 + Tavily keyless 一次 fallback / `SearchProvider` trait は抽象化のみ確定 / 3 層 AND / main process 実行 / metadata detail
- OpenCode V2（sst/opencode 10765ff2）prior art: keyless MCP endpoint JSON-RPC HTTP POST 同型。2 社対等振分（session checksum 半々）は不採用 — 既定 + 一次 fallback の直線構造のみ
- v02-network-guard が main process 層の NetworkGuard を提供し、session NetworkAccess mode の gate を担う（本 slice は依存として消費）

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/tools/`, `crates/runtime/`

Target part: builtin web_search tool。統一 Tool trait（name / schema / permissions / async execute）準拠、内部 transport は keyless MCP endpoint JSON-RPC、SearchProvider trait 抽象化、Exa keyless 既定 + Tavily keyless 一次 fallback

## In Scope

- 統一 Tool trait 準拠の `web_search` tool 実装（tool 名 `web_search`、引数 schema は `{query: string, num_results?: int, provider?: "exa"|"tavily"|"auto"}` を起点に実装 slice で確定）
- `SearchProvider` trait 抽象化の確定（`fn name` / `async fn search`）と、非 breaking な provider 追加可能性の mock provider 検証
- 内部 transport: keyless MCP endpoint JSON-RPC HTTP POST（OpenCode V2 同型）。キー提供時は provider REST API 直叩きへ切替可能な provider abstraction（credential_status として観測）
- Exa keyless 既定 → Tavily keyless 一次 fallback の直線構造（429 / 5xx / timeout で fallback）
- provider credential は環境変数経由・main process 環境のみ、worker sandbox / bwrap 内子プロセス env に非露出
- 実行権限 3 層 AND（role capability ∧ per-tool permission ∧ session NetworkAccess mode、gate は v02-network-guard の NetworkGuard 経由）と bwrap 外 main process 実行
- `ToolExecutor` 結果正規化層での `ContentOrigin` 機械導出（fail-closed、tool 自己申告不可。web_search は `WebUntrusted`）
- 既存 `ToolStarted` / `ToolCompleted` 継続 emit + `ToolCompleted` detail への metadata 追加（provider / request_id / latency_ms / result_count / used_fallback / fallback_attempts / credential_status + usage / cost 伝播）
- 制御マーカーエスケープは ToolExecutor 結果正規化層の既存機構を踏襲（ディスク書き込みはバイト一致の原則維持）

## Out Of Scope

- key 必須 provider（Brave / Kagi / Perplexity）の実装 — `SearchProvider` trait に乗る後続 slice
- provider-native routing（senpi 方式の LLM 依存 native search）— v0.3+。ContentOrigin / event 契約適用不可、pi#1432 観測欠落
- 汎用外部 MCP 接続（rmcp 本格運用）— 別 feature の Open question
- DuckDuckGo（完全 SERP なし）、SearXNG（self-host）、Mojeek / Startpage 等調査対象外
- cache layer、cross-session dedup
- `web_fetch` tool 本体 — 別 slice `v02-web-fetch-tool`
- SSRF / redirect ガード本体 — `v02-network-guard` の scope（本 slice は gate を消費する側）
- browser escalation — v0.2 不採用（capability facet `network.browser` は将来予約）

## Standalone Child Issue Contract

`turtton/evorch` で、統一 Tool trait（name / JSON Schema schema / permissions / async execute）に準拠する builtin `web_search` tool を `crates/tools/` に実装する。内部 transport は keyless MCP endpoint（Exa `https://mcp.exa.ai/mcp`）への JSON-RPC HTTP POST とし、Exa keyless 既定 → Tavily keyless 一次 fallback（`X-Tavily-Access-Mode: keyless`、429 / 5xx / timeout 時）の直線構造に留める（2 社対等振分は実装しない）。`SearchProvider` trait（`fn name` / `async fn search`）を確定し、Exa / Tavily 以外の provider が非 breaking に追加できることを mock provider で証明する。実行は 3 層 AND（role capability ∧ per-tool permission ∧ session NetworkAccess mode、gate は v02-network-guard の NetworkGuard）を通過した場合のみで、bwrap 外 main process で行う。provider credential は main process 環境変数のみで消費し、worker sandbox / bwrap 内子プロセス env へ露出しないことを unit test で証明する。`ToolExecutor` の結果正規化層で `ContentOrigin::WebUntrusted` を capability から fail-closed 機械導出し（tool 自己申告不可）、制御マーカーエスケープを適用する。既存 `ToolStarted` / `ToolCompleted` を継続 emit し（新規イベント種別なし）、`ToolCompleted` detail に provider / request_id / latency_ms / result_count / used_fallback / fallback_attempts / credential_status と取得可能範囲の usage / cost を載せる。key 必須 provider、provider-native routing、汎用 MCP 接続、cache layer、web_fetch 本体は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- `web_search` tool が統一 Tool trait（name / JSON Schema schema / permissions / async execute）に準拠し、ToolExecutor が `ToolStarted` / `ToolCompleted` を emit する
- 既定状態で Exa keyless により検索が動作し（API key 設定ゼロ）、fallback 発生時は Tavily keyless への一次 fallback で結果を返却する
- fallback 発生が `used_fallback` / `fallback_attempts` metadata で観測可能である
- provider API key 環境変数が main process のみで消費され、test sandbox / bwrap 内子プロセスの env に露出しないことを検証する unit test がある
- `ContentOrigin::WebUntrusted` が tool result envelope に fail-closed 機械導出され、tool 側から上書きできないことを検証する unit test がある
- 3 層 AND の各 deny 経路（role capability / per-tool permission / session NetworkAccess mode）が単独でも拒否することを検証する unit test がある
- `<system-reminder>` 等の制御マーカーを含む検索結果が ToolExecutor の結果正規化層でエスケープされる統合テストがある
- `SearchProvider` trait に Exa / Tavily 以外の provider を非 breaking に追加できることを mock provider（docs/テスト）で検証する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- Exa / Tavily keyless provider の unit test（wiremock で JSON-RPC 応答をモック）と fallback 遷移テスト（Exa 429 / 5xx / timeout → Tavily、`used_fallback` / `fallback_attempts` / `credential_status` metadata）
- credential 非露出 unit test（API key 環境変数は main process のみ、test sandbox / bwrap 内子プロセス env に露出しない）
- `ContentOrigin::WebUntrusted` の fail-closed 機械導出 unit test（tool 側上書き不可）
- 3 層 AND の各 deny 経路が単独でも拒否する unit test
- `<system-reminder>` 含有検索結果が ToolExecutor でエスケープされる統合テスト
- mock provider による `SearchProvider` trait 非 breaking 拡張の検証
- 既存 5 標準ツール・ToolExecutor・event stream 消費者の回帰確認（`ToolCompleted` detail 追加は下位互換）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md（v0.2 web ツール確定節）
- intents/evorch/decisions/0002-role-capability-boundaries.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0021-bwrap-linux-sandbox.md
- intents/evorch/interviews/web-tools-v02.json
- 兄弟 slice: `v02-web-fetch-tool`（web_fetch 本体）、`v02-network-guard`（NetworkGuard / SSRF・redirect 境界）

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` primary（v0.2 web ツール確定節へ反映）。supporting: ADR 0002 / 0008 / 0021、interviews/web-tools-v02.json
- ADR candidate: none（keyless transport と fallback 構造は grill web-tools-v02 で確定済み。ADR 0008 の延長）
- Diagram candidate: none
- Docs update: none（tool 追加の role-facing surface は Guide Reachability の route 宣言で扱う）
- Closeout writeback expected: yes。web_search 実装確定・`SearchProvider` trait の位置・fallback 挙動・metadata schema を tools-sandbox overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は Librarian（network allowed role、ADR 0002）が利用する公開 tool surface（`web_search`）を追加する。route: tools-sandbox overview で宣言される tool 利用 surface を guide が参照し、workspace / session の tool 設定に `web_search` が現れる。`no_role_facing_surface: false` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
