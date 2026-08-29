# v01-scaffold Implementation Packet

## Goal

v0.1 実装の土台となる Cargo workspace と開発基盤を整備する。`architecture.md` の「Rust Workspace 構成案（crates/）」に基づき、v0.1 で必要となる 10 crate とバイナリ crate の骨格（空 lib / 最小 main でよい）を `crates/` 配下に作成し、ルート `Cargo.toml`（virtual workspace）、`rust-toolchain.toml`、nix devShell（Rust toolchain 追加）、`.github/workflows/ci.yml`（fmt / clippy / test）を配置する。crate 分割の初期粒度は本 slice で決定し、ADR 0016 として記録する。

## Why

Rust 実装はまだ 1 行も存在せず、`cargo build` すら通らない状態。architecture.md の crate 構成は「案」に留まっており、Open questions「crate 分割の初期 granularity」が未解決。v0.1 の後続 packet（event-stream / session-storage / provider-client / agent-roles / tool-layer / routing / sandbox / gui）が `crates/` へ並行実装するための共通基盤（workspace ・toolchain ・devShell ・CI）を先に固定する必要がある。CI の2層検証運用（ADR 0015）も v0.1 の実装開始時点から前提となるため、`cargo fmt` / `clippy` / `test` を常時流す CI を最初から用意する。

## Scope

- ルート `Cargo.toml`: virtual workspace。`members = ["crates/*"]`。workspace 共通依存（serde / tokio / tracing 等のバージョン指定）を `workspace.dependencies` で管理。
- `crates/` 配下に中身は空の骨格を作成:
  - `runtime` / `event-bus` / `storage` / `providers` / `tools` / `sandbox` / `routing` / `model` / `config` / `gui`（lib crate: `Cargo.toml` + `src/lib.rs`）
  - `evorch`（バイナリ crate: `Cargo.toml` + `src/main.rs`）
  - crate 間依存は workspace レベルの path dependency で宣言可能な状態にしておく
- `rust-toolchain.toml`: toolchain channel と components（rustfmt / clippy）を固定。
- `flake.nix`: 既存 `devShells.default` を拡張し、Rust toolchain（cargo / rustfmt / clippy、必要に応じて rust-analyzer）を追加。
- `.github/workflows/ci.yml`: push / PR をトリガーに `cargo fmt --all --check` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace` を実行する workflow。
- 本 slice で crate セットを確定し、ADR 0016（crate 粒度）を host に生成する。

## Out of scope

- 各 crate の実ロジック（本 slice は骨格のみ。実装は event-bus / storage / provider-client 等他の packet で行う）。
- v0.1 外の crate（orchestration / agents / context / diagnostics / workspace-ui / gui-floem-proto 等）の作成。
- GUI framework の導入判断（ADR 0007 で egui + egui_dock が確定済み。`gui` crate は受入れ先として骨格だけ作る）。
- 実 provider 呼び出しや実 API 検証（ADR 0015 の第2層は CI 対象外）。
- guest 側 AGENTS.md / オンボーディング資料の編集。

## Verification

- スモーク検証: `cargo build --workspace` / `cargo test --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo fmt --check` がすべて通ること。
- `nix develop -c cargo build` 相当で devShell 内 toolchain が使えること。
- `git diff --check` で空白エラーがないこと。
- テストは骨格段階であるため unit test は最小限（バイナリ crate の smoke test 程度）でよい。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `intents/evorch/technology/architecture.md`（crate 構成案）と `intents/evorch/technology/mvp-roadmap.md`（v0.1 成功基準）を具体化する slice。新規 intent node は不要。
- ADR candidate: 必須採用。`intents/evorch/decisions/0016-crate-granularity.md`「v0.1 Cargo crate 分割の初期粒度を確定する」を本 slice で生成し、architecture.md の Open questions から crate granules 項目を解決済みにする。
- Diagram candidate: なし（architecture.md に crate 構成の text tree が既存。構成変更は ADR + テキスト更新で表現可能）。
- Docs update: `intents/evorch/technology/architecture.md` の「Rust Workspace 構成案」節に v0.1 初期セット確定を反映し、Open questions を更新。
- Closeout learning: crate セット選択の根拠（後続 v0.1 slice の依存関係から導出）と ADR 0016 へのリンクを記録。`write_back_required: true`。

- Guide reachability (G645): 本 slice は内部 crate 骨格と開発基盤のみで、ユーザー / オペレータ向け等の role-facing surface を追加しない。`no_role_facing_surface: true` を明示する。空欄のままにしない。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.