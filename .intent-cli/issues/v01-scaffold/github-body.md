## Goal

v0.1 実装の土台として、Cargo workspace・crate 骨格・Rust toolchain 固定・nix devShell・CI（fmt / clippy / test）を整備する。

## Why This Slice Exists Now

Rust 実装は未存在で `cargo build` すら通らない。architecture.md の crate 構成案は未確定のままで、v0.1 の後続 slice（event-stream / session-storage / provider-client / agent-roles / tool-layer / routing / sandbox / gui）が `crates/` に並行実装するための共通基盤を最初に固定する必要がある。

## Current Observed State

- Rust コードなし（`Cargo.toml` / `crates/` / `rust-toolchain.toml` / `.github/workflows/` すべて未作成）。
- `flake.nix` は既存（`devShells.default` に bashInteractive + intent-cli のみ。Rust toolchain は未含）。
- `flake.lock` / `.envrc` / `.gitignore` は既存。

## Accepted Baseline You May Assume

- flake.nix は既存のまま拡張できる（devShell に Rust toolchain を追加）。flake 再構築は不要。
- workspace ルートに `Cargo.toml`（virtual workspace, `members = ["crates/*"]`）を作成する。
- v0.1 初期 crate セット（本 packet で確定、ADR 0016）: `runtime` / `event-bus` / `storage` / `providers` / `tools` / `sandbox` / `routing` / `model` / `config` / `gui` + バイナリ crate `evorch`。
- 各 crate は空の骨格（`src/lib.rs`、バイナリは `src/main.rs`）でよい。
- ADR 0015（検証2層運用）により、CI には実 API 呼び出しを含めない。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `Cargo.toml, crates/, flake.nix, rust-toolchain.toml, .github/workflows/ci.yml`

Target part: Rust workspace の初期構成と開発基盤

## In Scope

- ルート `Cargo.toml`（virtual workspace、`workspace.dependencies` で共通依存管理）。
- 11 crate の骨格（runtime / event-bus / storage / providers / tools / sandbox / routing / model / config / gui / evorch）。
- `rust-toolchain.toml`（channel + components 固定）。
- `flake.nix` devShell への Rust toolchain 追加。
- `.github/workflows/ci.yml`（fmt --check → clippy -D warnings → test）。
- ADR 0016（crate 粒度）生成。

## Out Of Scope

- 各 crate の実ロジック（後続 packet の担当）。
- v0.1 外 crate（orchestration / agents / context / diagnostics 等）の作成。
- GUI framework の導入判断（ADR 0007 で確定済み）。
- 実 provider 呼び出し / 実 API 検証。

## Standalone Child Issue Contract

`turtton/evorch` に Rust の初期骨格を追加する: ルート virtual workspace `Cargo.toml`（`crates/*` を member にする）、`crates/` 配下に v0.1 の 10 lib crate（runtime / event-bus / storage / providers / tools / sandbox / routing / model / config / gui）とバイナリ crate `evorch` の空骨格（`Cargo.toml` + `src/lib.rs` or `src/main.rs`）、`rust-toolchain.toml` による toolchain 固定、`flake.nix` devShell への Rust toolchain 追加、`.github/workflows/ci.yml` で `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` を実行する。さらに、この packet の粒度決定を `intents/evorch/decisions/0016-crate-granularity.md` として書き、`intents/evorch/technology/architecture.md` の Open questions「crate 分割の初期 granularity」を解決済みに更新する。

## Acceptance Criteria

- `crates/` 配下に runtime / event-bus / storage / providers / tools / sandbox / routing / model / config / gui / evorch（バイナリ）の骨格が存在する。
- `cargo build --workspace` が通る。
- `cargo test --workspace` が通る。
- `cargo clippy --workspace -- -D warnings` が通る。
- `cargo fmt --check` が workspace 全体で通る。
- `rust-toolchain.toml` で channel と components（rustfmt / clippy）が固定されている。
- `nix develop` で devShell が立ち上がり、`cargo build` が実行できる。
- `.github/workflows/ci.yml` が fmt / clippy / test を実行する。
- ADR 0016（crate 粒度）が host に生成されている。

## Verification

- ローカルで `cargo build --workspace` / `cargo test --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo fmt --check` を実行し、すべて green を確認。
- `nix develop -c cargo build` 相当で devShell 内 toolchain を確認。
- `git diff --check` で空白エラーなし。

## Related Links

- [architecture.md](../../../intents/evorch/technology/architecture.md) — crate 構成案・技術スタック
- [mvp-roadmap.md](../../../intents/evorch/technology/mvp-roadmap.md) — v0.1 成功基準
- ADR 0007（GUI egui + egui_dock）/ ADR 0014（config アーキテクチャ）/ ADR 0015（検証2層運用）

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: 既存の technology/architecture.md・technology/mvp-roadmap.md を具体化。新規 intent node 不要。
- ADR candidate: `intents/evorch/decisions/0016-crate-granularity.md`「v0.1 Cargo crate 分割の初期粒度を確定する」（必須）
- Diagram candidate: なし
- Docs update: `intents/evorch/technology/architecture.md` の「Rust Workspace 構成案」節 + Open questions 更新
- Closeout writeback expected: yes（ADR 0016 生成 + architecture.md 更新）

## Guide Reachability (G645)

本 slice は内部 crate 骨格と開発基盤のみを追加し、guide 等の role-facing surface は追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.