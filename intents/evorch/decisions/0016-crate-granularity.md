# ADR 0016: v0.1 Cargo crate 分割の初期粒度

## Status

Accepted（2026-08-29、PR #10 で実装確定。issue #1 / v01-scaffold）

## Context

v0.1 実装の前に crate 骨格を固定する必要があった。architecture.md の構成案（`orchestration/` / `agents/` / `context/` / `diagnostics/` / `workspace-ui/` 等）は v0.1 全 slice を見越した目標構成で、実装が追っていない crate を最初から並べると並行実装時の `Cargo.toml` / `Cargo.lock` コンフリクトと未使用 crate の管理コストが増える。

## Decision

v0.1 の初期 crate セットは **10 lib crate + 1 バイナリ crate** とし、外部依存・クレート間依存ともにゼロの骨格で確定する。

| crate | 役割 |
|---|---|
| `runtime` | エージェントランタイムの中核（タスク実行・セッション制御） |
| `event-bus` | イベントストリームの内部配信基盤 |
| `storage` | セッション・ログ等の永続化 |
| `providers` | LLM provider クライアント抽象 |
| `tools` | ツール定義・実行レイヤ |
| `sandbox` | コマンド実行サンドボックス |
| `routing` | メッセージ・タスクの振り分け |
| `model` | 共有データモデル・型定義 |
| `config` | 設定読み込み・検証（ADR 0014。reload engine は本 crate 配下、ADR 0017 参照） |
| `gui` | egui ベース GUI（ADR 0007。骨格のみ） |
| `evorch` | バイナリエントリポイント |

- toolchain: rust 1.97.0（rust-toolchain.toml、components: rustfmt / clippy）、edition 2024、resolver 2
- architecture.md の目標構成（`orchestration/` / `agents/` / `context/` / `diagnostics/` / `workspace-ui/` 等）は v0.1 完了後の再編時に各 slice の実装内容に応じて導入する。v0.1 では上記 11 crate のみ

## Consequences

- 後続 slice（#2–#9）は `crates/` 配下で並行実装でき、`Cargo.lock` コンフリクトは最小化される
- 依存導入の集約ポイントとして `[workspace.dependencies]` を空セクションで先行確保済み
- 目標構成との差分（orchestration / agents / context / diagnostics / workspace-ui の未配置）は v0.1 後の workspace 再編で回収する。`event-bus` と `config` は構成案に無かった追加 crate（それぞれ ADR 0010 の event bus、ADR 0014 の config 層が独立 crate を要するため）
