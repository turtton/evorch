# Feature: Provider Routing（プロバイダルーティング）

[features 一覧](../) / [context-engine](../context-engine/overview.md) / [architecture](../../technology/architecture.md)

## 概要

Provider は Agent Runtime と完全に分離する。Agent SDK を中心に据えず、Claude / OpenAI / ChatGPT Codex / GitHub Copilot 等を普通の Model Provider として扱う。pi と同様に Provider ≠ API Protocol とする。

## 要件

- **Provider Type / Profile 分離**: type（anthropic / anthropic-subscription / openai / openai-codex / github-copilot / openrouter / openai-compatible）と credential instance を同一視しない。同一 type の上に複数 Profile を作れる（claude-personal / claude-business / copilot-work 等）
- **Model / Provider 分離**: Logical Model を上位概念にし、route → provider profile → API protocol に解決する（例: claude-main → claude-business → claude-personal → openrouter）
- **API Protocol 分離**: anthropic-messages / openai-responses / openai-completions / openai-codex-responses / google-generative-ai / copilot-compatible。ProviderProfile が protocol を選択する
- **Provider Capability**: prompt_cache / reasoning / tool_calling / compaction / streaming / transport の capability を明示する
- **fallback**: 単純 round-robin ではなく、current provider profile → same model / another profile → alternative logical model の順
- **Session affinity**: prompt cache のため同一 task / session で profile に留まる。429 / 5xx / timeout / quota / auth で cooldown 管理。Retry-After を優先
- **provider health / cooldown 管理**（v0.4 で拡張）

## 受け入れ基準

- provider type と profile を TOML で複数定義でき、logical model から解決できること
- fallback が「同じ model の別 profile → 別 logical model」の順で試行されること
- 同一 session で provider affinity が保たれ、失敗時のみ cooldown 付きで切り替わること

## Open questions

- subscription 系 provider（anthropic-subscription / openai-codex / github-copilot）の認証フロー詳細
- capability の未対応時の degrading 方針（cache 非対応 provider での扱い等）
