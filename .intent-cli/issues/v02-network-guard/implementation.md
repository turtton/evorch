# v02-network-guard Implementation Packet

## Goal

web_fetch / web_search が共有する NetworkGuard を、bwrap 外 main process 層（`crates/tools/` / `crates/runtime/`）に実装する。HTTPS 強制（http URL 受領時は https に upgrade、https 接続失敗時は http fallback せず error 返却）、redirect 最大 10 回で各 redirect 先 URL に同一ガードを再適用、DNS pinning（解決 IP への直接接続、TTL 非依存・同一リクエスト内でアドレス一貫性を担保、rebinding 対策）、IP range 判定（link-local 169.254.0.0/16 / CGNAT 100.64.0.0/10 / IPv6 link-local fe80::/10 を遮断、loopback 127.0.0.0/8・::1・`localhost` と RFC 1918 private を許可）、response size 三面チェック（Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計、5MB 枠）、3 層 AND 権限判定インタフェース（role capability ∧ per-tool permission ∧ session NetworkAccess）を提供する。worker sandbox の NetworkAccess 強制は引き締めたまま、web ツールのみ main process 経由で通信する実行構成とする。併せて reqwest redirect Policy カスタマイズ / hickory-resolver DNS pinning / ipnet range 判定の依存クレート feasibility 確認を実装タスクに含める。

## Why

v0.2 で Librarian の調査相棒として web_search / web_fetch を導入することが grill `web-tools-v02` で確定した（10/10 accepted、`intents/evorch/interviews/web-tools-v02.json`、tools-sandbox overview の v0.2 確定節）が、その通信境界は未実装である。bwrap の network 制御は all-or-nothing（ADR 0021）で per-destination 制御を sandbox 内に持てないため、q07 の確定通り web ツールは bwrap 外 main process 経由で通信し、SSRF guard も main process 層に集約する（v0.1.1 PR #20 の provider client パターン拡張）。guard なしで web ツールを許すと、link-local の cloud metadata endpoint（169.254.169.254）、DNS rebinding、解凍爆弾（gzip/deflate）への攻撃面が worker sandbox の外に開く。依存 baseline は landed 済みであり（統一 Tool trait 層の `v01-tool-layer`、fail-closed composition root の `v01-secure-tool-composition-root`）、本 slice はその上に NetworkGuard を載せる。q08 で厳格ガード + loopback/RFC 1918 許可（開発者の内部サービス到達のユーザー優先判断）+ link-local/CGNAT/IPv6 link-local 遮断が確定済みであり、ADR 0008 の v0.2 フェーズ（`ContentOrigin::WebUntrusted` 型付け）の前提となる通信境界でもある。

## Scope

- HTTPS 強制: http URL は https に upgrade し、https 接続失敗時は http fallback を拒否して error を返す
- redirect guard: 最大 10 回、redirect 先 URL も同一ガード（HTTPS / IP range 判定）を再適用する
- DNS pinning / rebinding 対策: 解決 IP に直接接続する（TTL 非依存、同一リクエスト内で IP 一貫）
- IP range 判定: 許可は loopback（127.0.0.0/8, ::1, `localhost`）と RFC 1918 private（10/8, 172.16/12, 192.168/16、開発者の内部サービス到達を叶える）。遮断は link-local 169.254.0.0/16（AWS/GCP metadata endpoint 含む）、CGNAT 100.64.0.0/10、IPv6 link-local fe80::/10
- response size 三面チェック: Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計（5MB 枠）
- bwrap 外 main process 層での実行構成: worker sandbox の NetworkAccess は引き締めたまま、web ツールのみ main process 経由で通信する
- 権限 3 層 AND: role capability（ADR 0002、network allowed）∧ per-tool permission（allow/ask/deny）∧ session NetworkAccess（Denied → 拒否 / OptIn → ask 承認 / Allowed → 通過）の判定インタフェース提供
- reqwest redirect Policy カスタマイズ / hickory-resolver DNS pinning / ipnet range 判定の依存クレート feasibility 確認を実装タスクに含め、選定結果を記録する

## Out of scope

- browser escalation、headless Chromium — 無関係。v0.2 は `network.browser` facet 予約のみで実装なし（q09）
- per-domain allowlist、proxy selective egress — v0.3+
- RSS / API 限定 fetch 等の用途特化 fetch
- web_search / web_fetch ツール本体（search provider transport、fetch extractor chain、`ContentOrigin` 型付け、model-facing 50KB truncation）— 本 slice は各ツールが共有する guard と判定インタフェースのみを提供する

## Verification

- unit tests: HTTPS upgrade と http fallback 拒否（error 返却）、IP range 判定（link-local 169.254.0.0/16〔169.254.169.254 を含む〕/ CGNAT 100.64.0.0/10 / IPv6 fe80::/10 の遮断、loopback 127/8・::1・`localhost` と RFC 1918 の許可）、DNS pinning による rebinding 防止（TTL の短い悪意ある DNS を想定した同一リクエスト内アドレス一貫性）、3 層 AND の各層 deny と OptIn ask の fail-closed 動作
- integration tests（wiremock 等 fixture サーバ）: redirect chain を張って最大 10 回と同一ガード再適用を検証、Content-Length 詐称両方向（大きく宣言→少なく送付 / 宣言なし→大きく送付）で 5MB guard が効くこと、gzip/deflate 解凍後累計チェックを含む
- 既存 runtime network / sandbox / tools テストの回帰なし（bwrap の二値 network 強制と provider client の main process 経路は不変）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` を primary とし、ADR 0002/0008/0021 を supporting とする。NetworkGuard の確定実装を overview へ反映する。新規 intent は不要
- ADR candidate: decline — ADR 0008/0021 の既存決定の実装接続であり新規 ADR 不要。facet `network.browser` 予約（q09）の明文化は overview 側で担保済み
- Diagram candidate: decline — guard 構成は feature overview の記述で十分
- Docs update: decline — 内部基盤層のみで role-facing surface を追加しない
- Closeout learning: NetworkGuard が main process 層で確定した事実・依存クレート選定結果（reqwest redirect Policy / hickory-resolver / ipnet）・feasibility 検証結果を tools-sandbox overview に記録する。`write_back_required: true`

- Guide reachability (G645): 内部基盤層（NetworkGuard / 判定インタフェース / 実行構成）のみで CLI / GUI / 対話 surface は追加しないため、`no_role_facing_surface: true` を宣言する

`improve` (G456 / G460) は later safety net。packet-time で writeback を宣言済み。
