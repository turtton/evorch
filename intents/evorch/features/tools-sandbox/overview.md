# Feature: Tools & Sandbox（ツールシステムとサンドボックス）

[features 一覧](../) / [orchestration](../orchestration/overview.md) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md)

## 概要

Tool は統一 interface（name / schema / permissions / execute）とし、Role ごとに capability を制限する。Shell / PTY と Sandbox もこの feature で扱う。

## 要件

- **Tool trait**: `fn name / fn schema / fn permissions / async fn execute` の統一 interface
- **初期 tool 実装候補**: read / write / edit / grep / glob / bash / git / diagnostics / definition / references / compact_context / delegate / delegate_background
- **Shell / PTY 分離**: 通常 command（cargo test / git diff / rg foo）は `tokio::process::Command`、interactive（ssh / REPL / interactive installer）は PTY（portable-pty 利用候補）
- **Code Intelligence**: LSP と Tree-sitter を独立機能として扱う。Tree-sitter（syntax-aware search / symbol extraction / AST navigation）、LSP（diagnostics / definition / references / hover / rename）
- **Sandbox**: agent ごとの能力に応じた sandbox policy（Codex 的）。Explorer = workspace read-only / network optional、Librarian = read-only / network allowed、Worker = workspace read-write / outside denied / network denied by default、Orchestrator = mutation tools unavailable。プラットフォーム候補: macOS Seatbelt / Linux Landlock・seccomp・namespaces(bwrap) / Windows restricted token・job object
- **MCP**: rmcp を利用

## 脅威モデル（ADR 0008、2026-08-29 確定）

OpenCode / pi / senpi は prompt injection を「防げない」と公式に明記し、Codex CLI 以外は OS-level sandbox を持たない調査結果を踏まえ、以下を段階導入する。

- **v0.1**: approval policy（on-request/on-failure/never）と OS enforcement（fs allowlist / network deny / workspace write scope）の二層分離（Codex 方式。承認しても sandbox 外は実行不可）／ credential を agent プロセス・子プロセス・環境変数へ渡さない（keychain 優先、0600 fallback）／ network egress は既定 deny で、role の network capability を bwrap network mode へ写像して強制（v0.1 は full-deny/full-open 二値: deny=`--unshare-net`、allow=親 netns 継承。per-destination allowlist は bwrap で不可、selective egress は v0.2）／ tool result 内の `<system-reminder>` 等の制御マーカーをエスケープ
- **v0.1 設計に組み込み・v0.2 実装**: tool result の `ContentOrigin` 型付け（RepositoryUntrusted / WebUntrusted / ToolTrusted 等）／ project trust（`AGENTS.md` / skills / MCP 設定等を未承認時はロードしない。pi/senpi の「trust 解決前に context file を読む」弱点は避ける）
- **v0.3**: untrusted mode（fs read-only / 一時コピー / network deny / credential 不可 / project 拡張無効）

既存 harness 同様、injection は「低減するが根除しない」ものと位置づける（product/overview.md の non-goals 参照予定）。

## v0.1 標準5ツールの実装確定（2026-08-29）

read / edit / grep / shell / git_diff の 5 ツールが `crates/tools/` にコード確定（PR #14、issue #5）。統一 `Tool` trait（`name` / JSON Schema `schema` / `permissions` / async `execute`）+ `ToolExecutor` が `ToolStarted` / `ToolCompleted` を event stream へ emit。要点:

- **制御マーカー エスケープの適用位置**: ディスク書き込みはバイト一致（ファイルは絶対に書き換えない）。エスケープ（`<system-reminder>` / `</system-reminder>` の `<` の直後に `\` 挿入、冪等）は `ToolExecutor` の結果正規化でのみ行う（ADR 0008 整合）
- **edit**: 同一親ディレクトリ上の一時ファイル + `persist` による atomic rename
- **shell**: 非 interactive = `tokio::process::Command`、interactive = portable-pty（one-shot）
- **引数検証**: 各ツールの JSON Schema で実施（新規依存 jsonschema 0.52 no-default-features）

## v0.1.1 role network 強制の実装確定（2026-08-30）

`NetworkAccess::{Denied, OptIn, Allowed}` → `SandboxNetworkMode::{DeniedNetwork, AllowFullNetwork}` の pure 写像（`OptIn` は明示 opt-in なしでは fail-closed で deny）を `crates/runtime/src/network.rs` に確定（PR #20、issue #19）。`build_sandbox(&ExecutionPolicy, workspace)` が policy → `BwrapConfig.allow_network` 伝播の composition seam であり、**production composition root からの呼び出しは `v01-secure-tool-composition-root` / `v01-gui-runtime-wiring` の責務**（v0.1 時点では repo 内に production composition root が存在せず、executor 構築は example のみ）。allow は destination filter を持たない full-open であり、型名・コメント・テスト名で誤魔化さない。provider client は main process 経路のまま bwrap 外。

bwrap integration test（`crates/sandbox/tests/bwrap.rs`、`crates/tools/tests/two_layer.rs`）の skip 観測方法: `#[ignore = "bwrap 実行環境が必要"]` 属性付き（既定 `cargo test` では pass ではなく ignored として集計され pass と区別可能）。bwrap 利用環境では `cargo test -- --include-ignored` で実行。子プロセス re-exec 系テストは `EVORCH_BWRAP_CHILD` 環境変数で二重 re-exec を防止。**follow-up**: CI に bwrap 利用 runner での `--include-ignored` job を追加する（T5。v0.1.1 scope 外で見送り）。

## v0.1.1 production composition root の実装確定（2026-08-30）

tool 実行の production 構築を fail-closed に閉じる composition root を `crates/sandbox/src/composition.rs` に確定（PR #22、issue #21）。`DirectSandbox` は private field `_sealed` で seal され `Default` derive を除去、隔離無効化は `DirectSandbox::new_unchecked()`（doc に非 production / テスト専用 opt-out を明記）経由のみ。production 入口は `sandbox::production_sandbox(BwrapConfig) -> Result<Arc<dyn Sandbox>, SandboxError>`（`BwrapSandbox::detect` 失敗時は error を返して DirectSandbox へ fallback しない）と `ToolExecutor::with_production_sandbox(event_bus, BwrapConfig) -> Result<Self, SandboxError>`。既存 `ToolExecutor::with_standard_tools` は挙動不変のまま doc により明示的低レベル注入 API として維持。`orchestrator_demo` は composition root 経由に移行済み（scripted flow / approval semantics は不変）。policy → network mode 伝播（`build_sandbox`）は `BwrapConfig` 入力として composition root と合成され、ExecutionPolicy からの consumer 配線は `v01-gui-runtime-wiring` の責務。bwrap 実環境テストは `#[ignore = "bwrap 実行環境が必要"]`（`crates/sandbox/tests/composition_root.rs`、`crates/tools/tests/production_sandbox.rs`）。

## v0.2 web ツール（web_search / web_fetch）の実装確定（2026-08-31、grill web-tools-v02）

v0.2 で Librarian の調査相棒として `web_search` / `web_fetch` を導入する（v0.2 roadmap の「Librarian/Oracle 追加」「ContentOrigin 実装」と対になる slice）。3 系統参考調査（OpenCode V2 / oh-my-opencode / pi・oh-my-pi・senpi）と interview `web-tools-v02`（10/10 accepted、`intents/evorch/interviews/web-tools-v02.json`）により以下が確定。

- **ツール構成（q01）**: `web_search` と `web_fetch` の 2 本分離。fetch を `read` に統合する方式（omp 由来）は不採用 — external untrusted コンテンツと local ファイルで `ContentOrigin` 型付け・権限境界が曖昧になるため。
- **検索の実装層（q02）**: builtin tool（統一 Tool trait 層）。内部 transport は keyless MCP endpoint 互換の JSON-RPC HTTP POST（OpenCode V2 同型）。キー提供時は provider REST API 直叩きへ切替可能な provider abstraction とする。汎用 rmcp client 層 / モデル依存 provider-native routing は v0.2 スコープ外。
- **provider 範囲（q03）**: Exa keyless 既定 + Tavily keyless 一次 fallback。`SearchProvider` trait の抽象化のみ v0.2 で確定、key 必須系（Brave/Kagi/Perplexity）は trait 上の後続 slice。credential は環境変数経由。
- **fetch 変換（q04）**: 明示 selector（`<article>`/`<main>`）優先 → Readability 系本文抽出 → full document fallback の extracter チェーン。`format: text | markdown | html` は参考系と同型（html は抽出スキップで生返却）。サイト専用 extractor（GitHub/arXiv 等）は v0.3+ backlog としてチェーン先頭に差し込める構造。
- **size 制限（q05）**: response 上限 5MB（Content-Length + 実読み + 解凍後）、model-facing 上限 50KB UTF-8 安全 byte-prefix 切詰め。超過は失敗でなく truncation metadata（`truncated: true, original_bytes: N`）を返し、続き取得ヒントを添付。context window 連動の動的切詰め（omo 方式）は v0.3+ backlog。
  - **spill-to-file 不採用の根拠**: untrusted web コンテンツを disk に落とすと「ローカルファイル」扱いで制御マーカー escape と `WebUntrusted` 型付けが外れ、ADR 0008 脅威モデルを実質破壊。worker の workspace 外 write denied とも衝突する。
- **ContentOrigin 型付け（q06）**: `ToolExecutor` の結果正規化層で tool が要求する capability から機械導出（fail-closed、tool 自己申告は不可）。mapping: network 要求 tool → `WebUntrusted`、workspace read tool → `RepositoryUntrusted`、その他 → `ToolTrusted`。Q2 の transport 経路と非依存で保証。
- **権限合成と実行位置（q07）**: 3 層 AND — role capability（ADR 0002、network allowed）∧ per-tool permission（allow/ask/deny）∧ session `NetworkAccess` mode（Denied → 拒否 / OptIn → 承認 / Allowed → 通過）。実行は bwrap 外 main process（v0.1.1 の provider client パターン拡張）。worker sandbox の NetworkAccess は引き締めたまま、web ツールのみ main process 経由で通信可能。provider credential は main process 環境変数のみ、worker sandbox 内非露出（ADR 0008 credential 分離の延長）。
- **SSRF/redirect ガード（q08）**: main process 層の `tools::NetworkGuard` に集約。`http` は接続前に `https` へ構造的 upgrade し、TLS 失敗時の HTTP fallback は持たない。reqwest の自動 redirect は `Policy::none()` で無効化し、最大 10 回の manual loop で各 `Location` を再ガードする。hickory-resolver 0.25 の system resolver を request-scoped cache で包み、redirect chain 全体で host ごとの初回解決結果を pin して reqwest custom `Resolve` から同じ IP へ直接接続する。ipnet 2 で link-local（169.254/16、AWS/GCP metadata endpoint）・CGNAT（100.64/10）・IPv6 link-local（fe80::/10）を遮断し、IPv4-mapped IPv6 は IPv4 へ正規化して同じ判定を行う。loopback（127/8, ::1, `localhost`）・RFC 1918 private IP（10/8, 172.16/12, 192.168/16、開発者の内部サービス到達のため）は許可する。response は Content-Length 事前・raw stream 累計・flate2 1 による gzip/deflate 解凍後累計の三面で 5MB を強制し、未知の Content-Encoding は fail-closed で拒否する。
- **browser escalation（q09）**: v0.2 では実装せず、capability facet を名前空間分離（`network` vs `network.browser`）して将来拡張点として予約。「network capability 保持 ≠ browser 実行可能」を型レベルで担保。
- **観測性（q10）**: 既存 `ToolStarted`/`ToolCompleted` を継続し、tool-specific metadata を `ToolCompleted` の detail に包含（新規イベント種別は追加しない）。web_search: provider / request_id / latency_ms / result_count / used_fallback / fallback_attempts / credential_status。web_fetch: url / final_url / status_code / content_length / decompressed_bytes / truncated / original_bytes / redirect_count / redirect_blocked / extraction_method。

web_search tool の確定実装（issue #43、PR #44、2026-09-02）: `crates/tools` の `tools::web_search::WebSearch`（Exa keyless 既定 + Tavily keyless 一次 fallback、`SearchProvider` trait は `crates/tools/src/search/` 内部抽象）として上記 q01-q03・q06-q07・q10 が実装で確定。keyless MCP transport は単発 `tools/call` + SSE/JSON content-type 分岐で足り、session handshake は不要（学び）。fallback trigger は 429 / 5xx / timeout のみ（`SearchError::is_fallback_trigger`、Q3 design lock）で連鎖しない一次 fallback。学び2: NetworkGuard の send error 畳み込みは timeout trigger を誤分類するため、HTTP layer 拡張時は **send error の timeout 種別を（`NetworkGuardError::Http` 経由で）保持すること**——本 slice で timeout 注入 ctor（`with_resolver_root_certificate_and_timeouts`）と POST redirect 拒否 error を guard に追加済み。production の layer-1 gate（`ExecutionPolicy::for_role`）は現行の全 role で web_search を Deny とし、consumer/composition slice での許可配線が残る（tripwire test `crates/runtime/tests/web_search_network_gate.rs` で固定）。

v0.3+ backlog: サイト専用 extractor、key 必須 provider、provider-native routing、context window 連動切詰め、credential/usage attribution 高度化。

## 受け入れ基準

- Role ごとに tool capability が runtime レベルで制限され、拒否が観測可能であること
- exec と pty が分離され、interactive process を扱えること
- sandbox policy が role ごとに適用されること（v0.1.1 で network が OS 強制まで接続（PR #20）、production composition root も landed（PR #22）。残る consumer 配線は v01-gui-runtime-wiring）
- web_search / web_fetch が v0.2 で Librarian から利用可能で、tool 実行が bwrap 外 main process で行われること、`ContentOrigin::WebUntrusted` が `ToolExecutor` 層で fail-closed に型付されること、truncation / fallback / redirect_blocked 等の metadata が `ToolCompleted` event detail に観測可能な形で流れること（v0.2 確定節参照）

## Related decisions

- [ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する](../../decisions/0002-role-capability-boundaries.md)
- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)
- [ADR 0021: Linux v0.1 sandbox 第一実装に bwrap を採用](../../decisions/0021-bwrap-linux-sandbox.md)

## Open questions

- ~~Linux sandbox の第一実装の選択（Landlock vs bwrap）~~ → 2026-08-30 解決（bwrap 採用、ADR 0021。Landlock は network 隔離不可のため不採用）
- MCP server の接続単位（session ごと / workspace ごと）
