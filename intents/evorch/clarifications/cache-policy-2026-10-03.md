# Cache-first: GUI の主表示を retention に変更（2026-10-03）

## 背景・観測

[2026-09-23 の補足](cache-policy-2026-09-23.md) で GUI の率も
`cache_hit_ratio = cache_read_tokens / input_tokens` を使うと定めた。
この率は各 run の初回要求が構造上 cold miss になるため、短い run ほど累積値が低く出る。
例えば 0% → 50% → 75% と正常に温まる3要求のサブエージェントは、
トークン加重の累積で約57%と表示される。キャッシュが壊れていないのに
利用者が不安になる見せ方であり、「cache は runtime health metric」という
[概要](../features/context-engine/overview.md) の意図とも合わない。

本補足は 2026-09-23 の「GUIの率も同じ分母を使う」だけを更新する。
`cache_hit_ratio`・`cache_retention_ratio` の定義、比較条件、`CacheRegression` の閾値は維持する。

## 決定

- GUI の主表示は retention とする。分母は 2026-09-23 の `previous_cache_tokens`
  （比較対象の実測 cache read+write）をそのまま使う。再利用可能トークン数は推定しない。
- provider は cache 観測が有効な各完了要求について、`RequestCompleted` の直前に
  `ProviderEvent::CacheReuseObserved` を1回発行する。比較できた場合は比較対象の
  request ID と `previous_cache_tokens`、比較できない場合は理由
  （比較対象なし・5分超・入力変更/compaction・比較対象の cache 量0・不正usage）を載せる。
- 1要求の retention は `min(cache_read, previous_cache_tokens) / previous_cache_tokens`。
  平均は比較できた要求だけで `Σmin(cache_read, previous) / Σprevious` とする。
  新たにキャッシュされた suffix の読み取りで1を超えた分は、表示上の retention に数えない。
  比較できない要求（初回など）は平均から外れるため、cold miss が率を下げない。
- 会話ヘッダは `cache 最新% (avg 平均%)`、サブエージェントは平均のみを表示する。
  最新要求に比較対象がなければ `—` とし、ツールチップに理由を示す。
  ほかの平均表記も `Δ` から `avg` に揃える（`Δ` は経過時間表示で使う）。
- 表示値が `CACHE_RETENTION_WARNING_THRESHOLD`（0.5、`CacheRegression` と共有）未満なら
  warning 色にする。会話ヘッダは最新値、サブエージェントは平均値で判定する。
- 従来の `cache_read / input` はツールチップに最新・平均として残す。
  エージェント一覧の compact 行からは外し、retention の平均に置き換える。

## 既知の制約

- cache write を報告しない provider（OpenAI 系）は初回要求の cache 量が0のため、
  2要求目は比較対象外（`PreviousUncached`）になる。
- 5分超の待機後のミスは比較対象外になり、retention には現れない。
  provider の TTL を保証しない保守的な比較窓という 2026-09-23 の方針による。

実装: `crates/event-bus/src/event.rs`、`crates/providers/src/observe/cache.rs`、
`crates/gui/src/model/cache_reuse.rs`。
