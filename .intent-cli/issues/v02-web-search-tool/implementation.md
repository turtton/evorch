# v02-web-search-tool Implementation Packet

## Goal

`crates/tools/` に builtin `web_search` tool を追加する。統一 Tool trait（`name` / JSON Schema `schema` / `permissions` / async `execute`）準拠で、内部 transport は keyless MCP endpoint（Exa: `https://mcp.exa.ai/mcp`）への JSON-RPC HTTP POST（OpenCode V2 同型）。`SearchProvider` trait 抽象化の下で Exa keyless 既定 → Tavily keyless 一次 fallback（`X-Tavily-Access-Mode: keyless`）の直線構造とし、キー提供時は provider REST API 直叩きへ切替可能にする。実行は bwrap 外 main process（v0.1.1 provider client パターンの延長）で、3 層 AND（role capability ∧ per-tool permission ∧ session NetworkAccess mode）を通過した場合のみ外に出る。`ToolExecutor` の結果正規化層で `ContentOrigin::WebUntrusted` を capability から機械導出（fail-closed、tool 自己申告不可）し、既存 `ToolStarted` / `ToolCompleted` に web_search 固有 metadata（provider / request_id / latency_ms / result_count / used_fallback / fallback_attempts / credential_status、取得可能範囲で usage / cost）を `ToolCompleted` detail として載せる。

## Why

v0.2 roadmap の「Librarian / Oracle 追加」と ADR 0008 v0.2 項目「ContentOrigin 実装」の対になる slice。ADR 0002 で Librarian は network allowed の調査 role と定義されているが、`crates/tools` には network を要求する tool が 1 つもなく、role capability が宣言のまま未接続である。grill session `web-tools-v02`（10/10 accepted、`intents/evorch/interviews/web-tools-v02.json`）で q01〜q10 が確定済みであり、本 packet はその実装落とし込みである。keyless 公式 hosted MCP は 2026-08 時点で Exa / Tavily の 2 社のみで、API key 設定ゼロで検索が動く zero-config UX が実現可能。pi#1432 の教訓（provider-native search で cost tracking / fallback 経路が観測不能になった）から、provider 経路・fallback・usage を event metadata で観測可能にすることを契約に含める。

## Scope

- 統一 Tool trait 準拠の `web_search` tool を `crates/tools/` に実装する（既存 5 標準ツールのパターン踏襲）。引数 schema は `{query: string, num_results?: int, provider?: "exa"|"tavily"|"auto"}` を起点に実装 slice で確定する
- `SearchProvider` trait（`fn name(&self) -> &str`、`async fn search(query, opts) -> Result<SearchResults, SearchError>`）を確定する。key 必須 provider（Brave / Kagi / Perplexity）は trait に乗る後続 slice であり、非 breaking 追加可能であることを mock provider で検証する
- 内部 transport: keyless MCP endpoint への JSON-RPC HTTP POST（OpenCode V2 同型、reqwest）。キー提供時は provider REST API 直叩きへ切替可能な provider abstraction とし、credential_status（keyless / key）として観測可能にする
- Exa keyless 既定 → Tavily keyless 一次 fallback の直線構造（429 / 5xx / timeout で fallback）。OpenCode V2 式 2 社対等振分（session checksum 半々）は採用しない
- provider credential は環境変数経由・main process 環境のみで消費する。worker sandbox / bwrap 内子プロセス env に非露出（ADR 0008 credential 分離の延長）
- 実行権限 3 層 AND: role capability（ADR 0002）∧ per-tool permission（allow / ask / deny）∧ session NetworkAccess mode（Denied → 拒否 / OptIn → 承認 / Allowed → 通過）。NetworkAccess gate は v02-network-guard が提供する NetworkGuard 経由で判定する
- 実行位置は bwrap 外 main process（q07 確定）。worker sandbox の NetworkAccess は引き締めたまま、web tool のみ main process 経由で通信可能にする
- `ContentOrigin` を `ToolExecutor` の結果正規化層で tool が要求する capability から機械導出する（fail-closed、tool 自己申告不可）。web_search は `WebUntrusted`。q06 の mapping（network 要求 tool → WebUntrusted / workspace read tool → RepositoryUntrusted / その他 → ToolTrusted）を同一導出層に定義する
- 既存 `ToolStarted` / `ToolCompleted` を継続 emit する（新規イベント種別は追加しない）。`ToolCompleted` に tool-specific metadata detail を下位互換で追加する: provider / request_id / latency_ms / result_count / used_fallback / fallback_attempts / credential_status（keyless / key）+ provider response の usage / cost 情報は取得できる範囲で伝播
- 制御マーカーエスケープは `ToolExecutor` 結果正規化層の既存機構（`sanitize::escape_control_markers`）を踏襲する。ディスク書き込みはバイト一致の原則を維持する（web_search はディスクに書かない）

## Out of scope

- key 必須 provider（Brave / Kagi / Perplexity）の実装 — `SearchProvider` trait に乗る後続 slice
- provider-native routing（senpi 方式の LLM 依存 native search）— v0.3+。Tool 層を通らず ContentOrigin / event 契約を適用不可（pi#1432 の観測欠落）
- 汎用外部 MCP 接続（rmcp 本格運用）— 別 feature の Open question（接続単位: session / workspace）
- DuckDuckGo（完全 SERP なし）、SearXNG（self-host）、Mojeek / Startpage 等の調査対象外 provider
- cache layer、cross-session dedup
- `web_fetch` tool 本体 — 別 slice `v02-web-fetch-tool`（q04 の変換パイプライン / q05 の size 制限はそちらで扱う）
- SSRF / redirect ガード本体 — `v02-network-guard` の scope（本 slice は gate を消費する側）
- browser escalation — q09 で v0.2 不採用。capability facet `network.browser` は将来予約

## Verification

- unit test: Exa / Tavily keyless provider（wiremock で JSON-RPC 応答をモック）、fallback 遷移（Exa 429 / 5xx / timeout → Tavily）、`used_fallback` / `fallback_attempts` / `credential_status` metadata
- unit test: provider API key 環境変数が main process のみで消費され、test sandbox / bwrap 内子プロセス env に露出しないこと
- unit test: `ContentOrigin::WebUntrusted` が tool result envelope に fail-closed 機械導出され、tool 側から上書きできないこと
- unit test: 3 層 AND の各 deny 経路（role capability / per-tool permission / NetworkAccess mode）が単独でも拒否すること
- 統合テスト: `<system-reminder>` を含む検索結果が `ToolExecutor` の結果正規化層でエスケープされること
- mock provider による `SearchProvider` trait 非 breaking 拡張の検証
- 既存 5 標準ツール・`ToolExecutor`・event stream 消費者の回帰確認（`ToolCompleted` detail 追加は下位互換）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` を primary とし、web_search の確定実装内容を v0.2 web ツール確定節へ反映する。新規 intent は不要（q01 / q02 / q03 / q06 / q07 / q10 の確定をそのまま実装に接続する）
- ADR candidate: decline — keyless MCP transport と「既定 + 一次 fallback」構造は tools-sandbox overview の v0.2 確定節（grill web-tools-v02）で確定済み。ADR 0008 credential 分離の延長であって新決定ではない
- Diagram candidate: decline — provider 選択 / fallback / 3 層 AND の経路は feature overview の記述で十分
- Docs update: decline — role-facing surface の追加は tool 登録であり、guide reachability の route 宣言（下記）で扱う
- Closeout learning: web_search tool の実装確定・`SearchProvider` trait の位置（crates/tools 内部抽象、key 必須 provider は後続 slice が trait に乗る）・fallback 挙動（`used_fallback` / `fallback_attempts` 観測）・metadata schema を overview に記録する。`write_back_required: true`

- Guide reachability (G645): Librarian（network allowed role、ADR 0002）が利用する公開 tool surface を追加するため `no_role_facing_surface: false`。route は tools-sandbox overview で宣言される tool 利用 surface → workspace / session の tool 設定

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
