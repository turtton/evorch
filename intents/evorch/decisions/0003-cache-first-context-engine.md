# ADR 0003: Cache-first Context Engine

## Status

Accepted

## Context

Prompt cache hit rate は後付け optimization として扱われることが多い。evorch では pi のような高い cache hit rate を目指し、cache を runtime correctness / health の一部として設計する。

## Decision

- Stable Prefix（system prompt / role definition / tool schema / project instruction snapshot / skill snapshot / memory snapshot）を毎 turn 再生成しない invariant とする。
- Append-only Context（user / assistant / tool）のみを追記する。
- 各 request で expected cacheable tokens / actual cache read tokens / cache hit ratio を記録する。
- cache hit ratio が急落した場合は `CacheRegression` を `DiagnosticBus` に流す。
- 長時間 command 実行中に cache TTL が切れないよう `cache-aware wait` を runtime primitive とする。
- Compaction は Agent が自己判断で呼ぶ control-flow primitive とする。

## Consequences

- Cache hit rate を計測し続ける必要がある。
- Stable Prefix の更新は明示的な `refresh_context` を通じて行う。
- Compaction は provider 固有実装（OpenAI Responses API 等）と抽象化の両方に対応する必要がある。

## Related

- [features/context-engine](../features/context-engine/overview.md)
- [features/storage-memory](../features/storage-memory/overview.md)
- [features/provider-routing](../features/provider-routing/overview.md)
- [features/diagnostics-self-improvement](../features/diagnostics-self-improvement/overview.md)
