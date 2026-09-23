# Cache-first 方針の実装語彙補足（2026-09-17）

> 更新: 指標・診断とツール出力の不変条件は
> [2026-09-23 の補足](cache-policy-2026-09-23.md) を参照。以下は2026-09-17時点の記録。

## 背景・対象

監査基準: `f907cc8d6628a61802c5a4ce25b78fcabba3bfcd`。
[ADR 0003](../decisions/0003-cache-first-context-engine.md) と
[Context Engine](../features/context-engine/overview.md) の語彙を補足する。
番号付き ADR は変更せず、未実装要件を達成済みに読み替えない。

## 問い・選択肢・結論

実装済みの仕組みを旧称に合わせて作り直すか、キャッシュ不変条件を保って
実装語彙を明確にするか。前者はバスやスナップショットの重複を招くため、後者を採る。
これは TTL、refresh、provider 固有 compaction、永続 health metrics の免除ではない。

- **DiagnosticBus** は独立した型を要求する名称ではなく、共通 `EventBus` 上の
  `DiagnosticEvent`（`code = CacheRegression`）として読む。
  根拠: `crates/providers/src/observe/cache.rs:39-59,130-141`。
- **cache hit ratio は二つの指標を区別する。** 診断用は actual cache-read /
  expected cacheable tokens。expected は最新の非 System message を除く本文と
  tools の UTF-8 bytes / 4（切り上げ）の概算で、tokenizer や cache residency の保証ではない。
  現行警告は送信前に warm と判定された scope の比率 `< 0.5`。warm は同じ
  bus/run/provider/profile/protocol/model の先行完了 request が cache read/write > 0
  を報告した場合のみ。履歴比率との差分による「急落」検知ではない。
  根拠: `crates/providers/src/observe/cache.rs:10-24,73-86,89-179`。
  GUI は `100 * cache_read / (input + cache_read + cache_write)` を表示する
  （`crates/gui/src/model/pricing.rs:20-27`）。この表示値を診断の 0.5 と比較しない。
  OpenAI は `input = prompt_tokens` と cached subset を別々に保持するため
  （`crates/providers/src/wire/openai/response.rs:87-95`）、GUI の分母を全 provider 共通の
  正規化済み入力 token 数とは扱わない。表示・課金の正規化は別の実装課題である。
- **snapshot は専用 snapshot 型や毎 request の再構築禁止を意味しない。** 通常 run の
  system は開始時に一度合成、tool specs は開始時に保持し、各 wire 変換で名前順に整列する。
  並び順の安定化だけでは内容の固定を代替できない。
  根拠: `crates/runtime/src/agent_loop.rs:160-166,214-228,450-505,624-640`、
  `crates/providers/src/wire/anthropic/convert.rs:73-86`、
  `crates/providers/src/wire/openai/request.rs:20-27`、
  `crates/providers/src/wire/codex/mod.rs:128-167`。
  [runtime-kernel の post-tool rules](../features/agent-runtime-kernel/overview.md) は
  対象ルールを再読して User message として末尾に追記する。既存 system の書換えではない
  （`crates/runtime/src/rules/api.rs:41-72`、`agent_loop/tool_calls.rs:667-671`）。
  「全 AGENTS.md を開始時に固定」は、この後続の追記機構まで禁止する意味にはしない。
- **compact_context** の実装上の操作名は `compact`。
  `crates/runtime/src/meta/mod.rs:57-74` と `meta/compaction.rs:10-25` が入口。
  抽象化は現在 `Summarizer` であり、provider 固有 Responses compaction の実装済みを意味しない
  （`crates/runtime/src/compaction/mod.rs:105-120`、`compaction/summary.rs:32-43`）。

## 実装範囲と未解決事項（要件は維持）

- 公式 OpenAI client は observation があると `prompt_cache_key = run_id`、互換 client は省略。
  provider profile の affinity と別の仕組みであり、session 全体の永続 affinity の代替ではない
  （`crates/providers/src/provider/openai.rs:57-66`、`openai/request.rs:5-15`、
  `openai_compatible.rs:37-45`、`crates/runtime/src/compose.rs:268-272`）。
- 全完了 request の expected/actual 比率は tracing に記録。SQLite は usage の1分集計と
  発行済み診断を保存するが、全 request の expected や比率の系列は保持しない。
  [ADR 0012](../decisions/0012-metrics-architecture.md) の downsampled-only 原則を維持し、
  health 比率の永続化不足を全 request の生ログ保存で埋めない
  （`crates/providers/src/observe.rs:129-156`、`observe/cache.rs:89-109`、
  `crates/gui/src/storage_bridge.rs:26-55`、`crates/storage/src/repo/metrics.rs:13-24`）。
- `refresh_context`、cache lease/TTL-aware wait、環境全体の context snapshot、provider 固有
  compaction、cache 診断閾値の設定公開は未実装のまま残す。一般的な shell timeout は
  lease-aware return ではない（`crates/tools/src/tools/shell.rs:301-323`）。
- Memory は `MemoryBoundary` を task 開始時に取得し、初回 User prompt に加える実装がある。
  System prefix への注入・関連度検索・全 session 終了での抽出が完成したとは扱わない
  （`crates/runtime/src/memory.rs:12-36`、`memory_queue.rs:42-62`、
  `runtime.rs:655-659`、`memory_lifecycle.rs:24-28`）。
