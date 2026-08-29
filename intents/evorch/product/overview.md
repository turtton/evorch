# Product Overview

[Mission・Vision・原則](../identity/mission.md) / [MVP ロードマップ](../technology/mvp-roadmap.md)

## これは何か

evorch は Rust 製の **AI-native Agent Harness / Agent Workbench** である。OpenCode + oh-my-opencode（omo）で得られる高品質なマルチエージェント体験をベースに、prompt cache、provider routing、sandbox、可観測性、自己改善を一級機能として統合した development environment を目指す。

中核は **Headless Agent Kernel** で、GUI はその上に乗る。Agent Runtime と UI は独立しており、GUI は event stream を購読するだけである。

## 目標ユーザー

- 複数の AI agent を並行させながら、その挙動・cache・provider 状態を自分の目で確認したい開発者
- 単一の chat UI や固定 workflow では物足りず、agent の能力を runtime レベルで制御・拡張したい開発者
- 複数 provider / 複数 account を使い分け、cache 効率とコストを重視する運用者

## ユーザージャーニー（概要）

1. ユーザーが依頼を入力する（調査・実装・レビュー・質問など何でも）
2. Intent Gate が Execution Shape（Direct / Coordinated）を決定する
3. Coordinated なら Orchestrator が動的に agent topology を構築し、必要に応じて background agent を起動する
4. ユーザーは GUI 上で全 agent の status / role / model / provider / reasoning / tool execution / transcript / cache / usage を並列して観測する
5. Task 境界で compact が行われ、次の task に context を引き継ぐ
6. 不具合は runtime が検出し、自動で Issue 化される

## Non-goals

- **OpenCode の単なる代替ではない**: chat-first の coding agent の再実装は目指さない
- **固定 workflow の強制**: Explore→Plan→Execute→Review→Fix のような状態機械を中核にしない
- **Chat UI**: TUI / 単一ウィンドウのチャットに限定しない
- **特定ベンダー依存**: 特定モデル・provider・orchestration に依存しない
- **初版から全機能**: v0.1 は最小構成で始め、段階的に拡張する

## 最終イメージ

> **複数の専門 Agent が並行して活動し、その状態・判断・cache・provider・tool execution を人間が常時観測できる、自己改善可能な Native Agent Workbench**

詳細は [features/](../features/) 配下の各 feature overview と [technology/architecture.md](../technology/architecture.md) を参照。
