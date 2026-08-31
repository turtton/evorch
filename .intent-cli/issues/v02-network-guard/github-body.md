## Goal

web_fetch / web_search が共有する NetworkGuard を bwrap 外 main process 層に実装する。HTTPS 強制（http は https に upgrade、https 失敗時は http fallback せず error 返却）、redirect 最大 10 回で各 redirect 先に同一ガード再適用、DNS pinning（解決 IP への直接接続、TTL 非依存・同一リクエスト内アドレス一貫性）、IP range 判定（link-local / CGNAT / IPv6 link-local を遮断、loopback / RFC 1918 private を許可）、response size 三面チェック（Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計、5MB 枠）、3 層 AND 権限判定インタフェースを提供する。worker sandbox の NetworkAccess 強制は緩めない。

## Why This Slice Exists Now

v0.2 で Librarian の調査相棒として web_search / web_fetch を導入することが grill `web-tools-v02`（10/10 accepted）で確定した。q07 で権限判定は 3 層 AND、実行位置は bwrap 外 main process（v0.1.1 PR #20 の provider client パターン拡張）と確定し、q08 で SSRF 境界は厳格ガード + loopback/RFC 1918 許可 + link-local/CGNAT/IPv6 link-local 遮断と確定している。bwrap の network 制御は all-or-nothing（ADR 0021）で per-destination 制御を sandbox 内に持てないため、guard は main process 層に集約するしかない。この境界を先に実装しなければ、web ツールの許可が cloud metadata endpoint（169.254.169.254）、DNS rebinding、解凍爆弾への攻撃面を直接開く。ADR 0008 の v0.2 フェーズ（`ContentOrigin::WebUntrusted` 型付け）の前提となる通信境界でもある。

## Current Observed State

- `crates/tools/src/executor.rs:92-108` の `with_standard_tools` が登録するのは read / edit / grep / shell / git_diff の 5 ツールで、web ツールも NetworkGuard も存在しない
- `crates/tools/src/tool.rs:11-18` の `Permissions` は fs_read / fs_write / process_spawn の 3 フラグのみで、network facet の表現がない
- `crates/runtime/src/network.rs:16-40` の `SandboxNetworkMode`（Unshared / ParentNetns）と `sandbox_network_mode` 写像は sandbox レベルの二値強制のみで、main process 側の通信ガードは存在しない
- workspace `Cargo.toml:25` に reqwest 0.12（default-features = false / json / stream / rustls-tls）があるが、利用は providers（provider API 呼び出し）と model のみ。hickory-resolver / ipnet は未導入
- bwrap の network 制御は `--unshare-net` か親 netns 継承の all-or-nothing（ADR 0021）で、sandbox 内に per-destination 制御は存在しない

## Accepted Baseline You May Assume

- ADR 0002: role は capability boundary。Librarian は network allowed、Worker は denied 等、role capability が 3 層 AND の第 1 層の判定材料
- ADR 0021: bwrap の network 制御は all-or-nothing。v0.1.1 は deny=`--unshare-net` / allow=親 netns 継承。per-endpoint 制御は sandbox 内で不可能 — web ツールは main process 層経由（q07 確定）
- ADR 0008: credential は agent プロセス・子プロセス・環境変数へ渡さない方針の延長で、web ツールの provider credential は main process 環境変数のみで worker sandbox 内に非露出。`ContentOrigin`（WebUntrusted）は v0.2 実装フェーズ
- grill web-tools-v02 の確定回答（`intents/evorch/interviews/web-tools-v02.json`）: q07（3 層 AND + bwrap 外 main process 実行、worker sandbox の NetworkAccess は引き締めたまま）、q08（厳格 SSRF ガード + loopback/RFC 1918 許可 + link-local/CGNAT/IPv6 link-local 遮断）、q09（browser escalation は v0.2 実装なし、`network` と `network.browser` の facet 名前空間分離で予約）
- v0.1.1 の provider client パターン（PR #20、main process 実行 + per-call auth）を web ツールへ拡張する既定路線
- reqwest 0.12 / wiremock 0.6 は workspace 既存依存

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/tools/`, `crates/runtime/`

Target part: main process 層 (bwrap 外) で web_fetch / web_search が共有する NetworkGuard — HTTPS 強制 / redirect 再検証 / DNS pinning / IP range 判定 / size 両面チェック

## In Scope

- HTTPS 強制: http URL は https に upgrade、https 接続失敗時は http fallback を拒否して error 返却
- redirect guard: 最大 10 回、redirect 先 URL も同一ガードを再適用
- DNS pinning / rebinding 対策: 解決 IP に直接接続（TTL 非依存、同一リクエスト内で IP 一貫）
- IP range 判定: 許可 = loopback（127.0.0.0/8, ::1, `localhost`）と RFC 1918 private（10/8, 172.16/12, 192.168/16、開発者の内部サービス到達を叶える）。遮断 = link-local 169.254.0.0/16（AWS/GCP metadata endpoint 含む）、CGNAT 100.64.0.0/10、IPv6 link-local fe80::/10
- response size 三面チェック: Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計（5MB 枠）
- bwrap 外 main process 層での実行構成: worker sandbox の NetworkAccess は引き締めたまま、web ツールのみ main process 経由で通信
- 権限 3 層 AND: role capability（ADR 0002、network allowed）∧ per-tool permission（allow/ask/deny）∧ session NetworkAccess（Denied → 拒否 / OptIn → ask 承認 / Allowed → 通過）の判定インタフェース提供
- reqwest redirect Policy カスタマイズ / hickory-resolver DNS pinning / ipnet range 判定の依存クレート feasibility 確認を実装タスクに含め、選定結果を記録

## Out Of Scope

- browser escalation、headless Chromium — 無関係。v0.2 は `network.browser` facet 予約のみで実装なし（q09）
- per-domain allowlist、proxy selective egress — v0.3+
- RSS / API 限定 fetch 等の用途特化 fetch
- web_search / web_fetch ツール本体（search provider transport、fetch extractor chain、`ContentOrigin` 型付け、model-facing 50KB truncation）— 本 slice は各ツールが共有する guard と判定インタフェースのみを提供する

## Standalone Child Issue Contract

`turtton/evorch` に、web_fetch / web_search が共有する NetworkGuard を bwrap 外 main process 層（`crates/tools/` / `crates/runtime/`）へ実装する。http URL は https に upgrade し、https 接続失敗時は http fallback せず error を返す。redirect は最大 10 回で、各 redirect 先 URL に同一ガード（HTTPS / IP range 判定）を再適用する。DNS pinning で解決 IP に直接接続し（TTL 非依存、同一リクエスト内でアドレス一貫性）、rebinding を防ぐ。IP range 判定は link-local 169.254.0.0/16（AWS/GCP metadata endpoint 169.254.169.254 を含む）、CGNAT 100.64.0.0/10、IPv6 link-local fe80::/10 を遮断し、loopback（127.0.0.0/8, ::1, localhost）と RFC 1918 private（10/8, 172.16/12, 192.168/16）を許可する。response size は Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計の三面で 5MB を強制する。権限は 3 層 AND 判定インタフェース（role capability ∧ per-tool permission ∧ session NetworkAccess: Denied → 拒否 / OptIn → ask 承認 / Allowed → 通過）を fail-closed で提供する。worker sandbox の NetworkAccess 強制は緩めず、web ツールの通信は main process 経由のままとする。reqwest redirect Policy カスタマイズ / hickory-resolver DNS pinning / ipnet range 判定の feasibility を実装タスクに含め、選定結果を記録する。unit test（HTTPS / IP range / DNS pinning / 3 層 AND）と fixture サーバによる integration test（redirect chain、Content-Length 詐称両方向の 5MB guard）で検証する。browser escalation、per-domain allowlist、proxy selective egress、RSS / API 限定 fetch は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- HTTPS 強制の upgrade（http URL 受領時は https へ upgrade）と https 接続失敗時の http fallback 拒否（error 返却）が unit test で検証済み
- redirect 最大 10 回と、redirect 先 URL への同一ガード再適用が integration test で検証済み（fixture サーバで redirect chain を張る）
- DNS pinning 実装が rebinding 攻撃（TTL の短い悪意ある DNS）を防ぐことを unit test で検証（解決 IP への直接接続、同一リクエスト内でアドレス一貫性を担保）
- link-local 169.254.0.0/16（AWS/GCP metadata endpoint 169.254.169.254 を含む）/ CGNAT 100.64.0.0/10 / IPv6 link-local fe80::/10 の遮断と、loopback（127.0.0.0/8, ::1, localhost）および RFC 1918 private（10/8, 172.16/12, 192.168/16）の許可が IP range 判定 unit test で検証済み
- Content-Length 詐称（大きく宣言して少なく送付 / 宣言なしで大きく送付）の両方で 5MB guard が効く統合テスト（Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計の三面チェック）
- 3 層 AND 判定（role capability ∧ per-tool permission ∧ session NetworkAccess）で各層の deny と OptIn の ask 承認が fail-closed 動作することを検証
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass

## Verification

- unit tests: HTTPS upgrade と http fallback 拒否、IP range 判定（遮断 3 種 + 許可 2 系統、169.254.169.254 を含む）、DNS pinning による rebinding 防止、3 層 AND の各層 deny と OptIn ask の fail-closed 動作
- integration tests（wiremock 等 fixture サーバ）: redirect chain を張って最大 10 回と同一ガード再適用を検証、Content-Length 詐称両方向で 5MB guard が効くこと、gzip/deflate 解凍後累計チェックを含む
- 既存 runtime network / sandbox / tools テストの回帰なし（bwrap の二値 network 強制と provider client の main process 経路は不変）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md（v0.2 web ツール実装確定節）
- intents/evorch/interviews/web-tools-v02.json（q07 / q08 / q09）
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0021-bwrap-linux-sandbox.md
- intents/evorch/decisions/0002-role-capability-boundaries.md
- 前提 slice: v01-tool-layer、v01-secure-tool-composition-root

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` primary、ADR 0002/0008/0021 supporting
- ADR candidate: none（ADR 0008/0021 の既存決定の実装接続。facet `network.browser` 予約の明文化は overview 側で担保済み）
- Diagram candidate: none
- Docs update: none（role-facing surface なし）
- Closeout writeback expected: yes。NetworkGuard が main process 層で確定した事実・依存クレート選定結果（reqwest redirect Policy / hickory-resolver / ipnet）・feasibility 検証結果を tools-sandbox overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は内部基盤層（NetworkGuard / 3 層 AND 判定インタフェース / main process 実行構成）のみを変更し、CLI / GUI / 対話 surface を追加しない。`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
