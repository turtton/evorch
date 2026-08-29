# ADR 0004: Provider Type / Profile / Logical Model / API Protocol の分離

## Status

Accepted

## Context

特定の Agent SDK を中心に据えると、Claude / OpenAI / GitHub Copilot 等を一律に扱えなくなる。pi と同様に Provider を普通の Model Provider として扱い、抽象化したい。

## Decision

以下の 4 層に分離する。

```text
logical model
  ↓
route
  ↓
provider profile
  ↓
API protocol
```

- **Provider Type**: anthropic / anthropic-subscription / openai / openai-codex / github-copilot / openrouter / openai-compatible
- **Provider Profile**: Provider Type 上の credential instance。同一 type に複数 profile を作れる（claude-personal / claude-business / copilot-work 等）
- **Logical Model**: 抽象化されたモデルクラス（claude-class / gpt-class 等）
- **API Protocol**: Provider とは独立（anthropic-messages / openai-responses / openai-completions 等）

fallback は current provider profile → same model / another profile → alternative logical model の順とする。Session affinity は prompt cache のために同一 task / session で同一 profile に留まる。

## Consequences

- Provider 追加・入れ替えが容易になる。
- capability（prompt_cache / reasoning / tool_calling / compaction / streaming / transport）を provider ごとに明示する必要がある。
- 認証フローが provider 種別ごとに異なるため、profile 設定が複雑になる。

## Related

- [features/provider-routing](../features/provider-routing/overview.md)
- [features/context-engine](../features/context-engine/overview.md)
- [features/agent-runtime-kernel](../features/agent-runtime-kernel/overview.md)
