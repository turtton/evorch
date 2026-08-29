# Feature: Context Engine（cache-first コンテキストエンジン）

[features 一覧](../) / [provider-routing](../provider-routing/overview.md) / [storage-memory](../storage-memory/overview.md)

## 概要

Prompt cache hit rate は後付け optimization ではなく、Runtime correctness の一部として設計する。pi のような高い cache hit rate を狙う。

## 要件

- **Stable Prefix**: system prompt / role definition / tool schema / project instruction snapshot / skill snapshot / memory snapshot からなる prefix を毎 turn 再生成しない。AGENTS.md / skills / memory / environment / tool schema は task 開始時に snapshot 化して固定する。`refresh_context` で明示的に cache invalidation する
- **Append-only Context**: Stable Prefix の後に user / assistant / tool の message を追記するのみ
- **Cache metrics**: 各 request で expected cacheable tokens / actual cache read tokens / cache hit ratio を記録する。急落した場合は CacheRegression として DiagnosticBus に流す。cache は billing metric ではなく runtime health metric
- **Cache-aware wait**: 長時間 command 実行中に prompt cache TTL が切れないよう、JobHandle で待機し cache lease 期限切れが近づいたら agent turn に戻る。tool call 自体を cache TTL より長く block させない（Senpi の cache-aware wait を runtime primitive にする）
- **Compaction**: Agent が自分で判断して呼べる control-flow primitive（compact_context）。context checkpoint を更新して agent resume。provider 固有 compaction は `trait Compactor` で抽象化し、OpenAI / GPT 系は公式 Responses API の compaction を優先
- **Memory**: task / session 終了時に quick agent が「将来も有用な知識」を抽出して persistent memory へ保存。session 途中で stable prefix に挿入せず、次の task boundary から利用（Relevant Memory Retrieval → Memory Snapshot → Stable Prefix）

## 受け入れ基準

- Stable Prefix がターン間で不変であり、cache hit ratio が計測・記録されること
- cache hit ratio が閾値を下回った場合に CacheRegression 診断が発行されること
- compact_context が control-flow primitive として動作し、checkpoint から resume できること

## Related decisions

- [ADR 0003: Cache-first Context Engine](../../decisions/0003-cache-first-context-engine.md)
- [ADR 0004: Provider Type / Profile / Logical Model / API Protocol の分離](../../decisions/0004-provider-routing-separation.md)

## Open questions

- cache TTL の各 provider 差異の抽象化方法
- compaction の要否を agent が判断する基準の初期実装
