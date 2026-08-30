# v01-secure-tool-composition-root Implementation Packet

## Goal

tool execution の production composition root を fail-closed にする。現在は `crates/sandbox/src/exec.rs:30-43` の `DirectSandbox` が public unit struct かつ `Default` で、`crates/sandbox/src/lib.rs:13-20` から再 export され、任意の呼び出し側が policy 明示なしに OS isolation を無効化できる。`crates/runtime/examples/orchestrator_demo.rs:196-206` も `Arc::new(DirectSandbox)` を `ToolExecutor::with_standard_tools` へ直接注入している。これを、production は workspace root と明示 policy から `BwrapSandbox` を構築し、detect failure では error を返す composition root 経由に改める。DirectSandbox が必要な unit tests / deliberate opt-out は、意図が API 上で明示される限定経路に封じる。

## Why

v0.1 inspect で、issue #6 / `v01-sandbox-approval` が実装した fail-closed bwrap と issue #5 / `v01-tool-layer` の dependency injection の間に、安全な production construction が存在しないことが判明した。ADR 0021 は bwrap detect failure で危険ツールを実行せず、隔離なし fallback 経路は存在しないと定める（`intents/evorch/decisions/0021-bwrap-linux-sandbox.md:17-20`）。しかし現 API は `DirectSandbox` の公開構築をコンパイル時に許し、demo が実際にその path を採るため、呼び出し側の一行で二層分離を迂回できる。sandbox execution semantics を変更せず、composition boundary を閉じる必要がある。

## Scope

- `DirectSandbox` の unit-like public construction と無条件 `Default` を廃止する。選択肢は private field + named explicit opt-out constructor、sealed/test-only constructor、または deny-by-default policy token を必須にする API とし、「直接実行である」意図を呼び出し側に残す
- production composition root を `crates/sandbox` または `crates/tools` の責務が明確な module に追加し、workspace root / sandbox policy を入力として `BwrapConfig` → `BwrapSandbox::detect` → `ToolExecutor::with_standard_tools` を構築する
- bwrap 不在・機能確認失敗は `SandboxError::BwrapUnavailable` 等の typed error として返し、DirectSandbox へ fallback しない
- `ToolExecutor::with_standard_tools(event_bus, Arc<dyn Sandbox>)` の低レベル注入 API は tests / custom wiring で必要なら維持できるが、production-safe constructor と名前・visibility・docs で明確に区別する
- `crates/runtime/examples/orchestrator_demo.rs` を新 composition root に移し、scripted model / event flow / runtime behavior を維持する。demo の実行には bwrap と workspace root が必要であることを error/context で明示する
- compile-fail（trybuild 等）または visibility/API contract test で policy なしの permissive construction が不可能であることを固定する

## Out of scope

- `Sandbox::wrap`、bwrap argv、filesystem mounts、network namespace、environment allowlist の semantics 変更
- approval policy / `ToolExecutor::execute` の分類・承認待機・event emission の変更
- gVisor / Landlock / seccomp 等の新 backend
- macOS / Windows sandbox 実装
- runtime topology、model provider、routing の変更

## Verification

- API contract test: downstream-style codeが `DirectSandbox` を policy 明示なしで構築できない（compile-fail または privacy assertion）
- composition root unit tests: valid workspace + available bwrap で standard ToolExecutor を構築し、bwrap unavailable / detect failure では typed error を返して fallback しない
- explicit opt-out test: tests 等で必要な DirectSandbox 経路は意図を示す API を通した場合だけ利用できる
- tool tests: shell / git_diff の既存 wrap と result semantics、approval の既存 tests が回帰しない
- `cargo run -p runtime --example orchestrator_demo` 等の既存 demo surface を実行し、deterministic flow が完了する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md`。二層分離の construction invariant を既存 feature に追記する。新規 intent は不要
- ADR candidate: decline — ADR 0021 の fail-closed decision を API に適用する修正で、新しい後戻り困難な技術選択ではない
- Diagram candidate: decline — composition root の一方向構築は文章と API docs で十分
- Docs update: decline — internal constructor/API の変更で role-facing surface は追加しない
- Closeout learning: production composition root の API、DirectSandbox の限定 opt-out、bwrap unavailable error surface を記録する。`write_back_required: true`

- Guide reachability (G645): 内部 crate の construction API と demo wiring の変更のみで role-facing guide surface は追加しない。`no_role_facing_surface: true`

`improve` (G456 / G460) は later safety net。packet-time で writeback を宣言済み。

## Closeout learning（2026-08-30、v01-role-network-enforcement / PR #20 より）

`crates/runtime/src/network.rs` の `build_sandbox(&ExecutionPolicy, workspace)` が policy → `BwrapConfig.allow_network` 伝播の composition seam として存在する。本 unit では production composition root からこの関数を**必ず呼び**、role ごとの network mode（Denied = `--unshare-net`、Allowed = 親 netns 継承＝full-open）が sandboxed tool execution（shell / git_diff）へ伝播することを検証要件に含めること。allow は destination filter 非対応の full-open であり、provider client は main-process 経路のまま bwrap 外であることは変更しない。
