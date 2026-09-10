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

## v0.11 (v06-model-catalog-preset): models.dev カタログ + ModelPreset + compaction window 正確化

### 背景

deepseek-v4-flash-0731 で 413 Payload Too Large が発生。 compaction の `context_window_tokens` が DEFAULT 200k 固定で、モデルごとの実際の limit を参照する仕組みがなかったため。また provider 別に同一モデルを個別設定する構造的な不便があった。

### 実装内容 (P1-P4)

- P1 config (3b670b3): `[model_presets.<name>]` (context_window/max_output_tokens/pricing) と `ModelEntryConfig` 拡張 (metadata_source/metadata_ref/preset/context_window、全 optional)。legacy 文字列/id+enabled 互換維持。schema artifact regen 済み
- P2 catalog (14fe463): 新規 `crates/catalog`。models.dev `/api.json` fetch、24h TTL cache (atomic write+file lock)、stale 利用+バックグラウンド refresh、fetch timeout 10s。13 テスト全ネットワーク非依存。実 API 動作も確認 (GPT-4o context=128000)
- P3 runtime (5a408a2): 解決レイヤ (manual → preset → catalog → None)。catalog は初回 run 時に非同期ロード。方式 A で解決 window を `model_overrides` に注入し compaction に反映。`model` / `profile/model` 両形式対応
- P4 GUI (0da9773): provider 設定で preset 選択・metadata_source・models.dev 参照・context_window override、解決値表示 (解決元付き)、Refresh ボタン + 最終更新時刻。config serializer のメタデータ欠落修正 (内部実装のみ)

### 副次修正

- 387d801: shell contract allowlist の `sh -c` unwrap (ad83290 以降 shell_delivery の `gh pr merge` が deny されていた既存 regression)。delivery adapter の出力抽出も新形式 `exit_code: 0` に対応
- 1adccb6: routing の ModelEntryConfig テスト initializer を struct update syntax に統一

### 検証

- workspace テスト 23 group green (残は既知 flake `headless_run_completes_with_single_mock_response` のみ、単体実行で pass)
- clippy / fmt clean

### 残課題 (open questions)

- models.dev provider ID ↔ evorch provider 名 (Crof 等) のマッピング方法
- models.dev に存在しないマイナーモデルの fallback (preset 必須?)
- cache 更新時の preset 整合性チェック

## v0.12 (UX 改善バッチ): catalog 自動フォールバック + read エイリアス + モデル編集 UI 高さ緩和

### 背景

v06 実装後の実機試用で3点の体験問題が発覚: (1) metadata_source 未設定時に catalog 自動解決されずユーザー手動入力が前提だった、(2) モデル編集エリアが狭く設定困難、(3) thread-2 の read ツールが理由不明のままエラー連発。

### 実装内容

- c468ccb (UX-A): metadata_source が未設定/Manual/ProviderDefault でも catalog を自動探索。`deepseek-v4-flash` → `deepseek/deepseek-v4-flash` の `/` 境界 suffix 一致で一意候補のみ解決 (曖昧性は拒否)。優先順位 manual→preset→catalog→default を維持。GUI の価格/制限値も共通検索に統一し、未解決時 `Unknown ctx (default)` 案内表示
- b224c3e (UX-C): read tool が `file`/`file_path`/`filename`/`target` エイリアスを許容 (path 優先)。Executor の全失敗経路 (検証/実行/承認拒否) で `ToolCompleted.output` にエラー文を記録 — 従来は `detail: null` で output 欠落し、GUI/ログに理由が残らなかった
- 81775d6 (UX-B): provider 設定の設定済みモデル一覧で内側 ScrollArea を除去し自然高に (外側スクロールに集約)。取得済み一覧の上限 100→200px

### thread-2 エラーの真因 (実機 DB 調査)

`~/.config/evorch/evorch-events.db` の ToolStarted で、LLM が read に `{"file": "..."}` を送信 → `path` required で InvalidArgs 連発。途中で `path` に気づき成功に至っていた。UX-C でエイリアス + エラー可視化の両方を修正。

### 検証

- workspace 23 group green (既知 flake `headless_run_completes_with_single_mock_response` のみ、単体 pass)
- agents_headless 2 件等の既存失敗は変更前 HEAD でも再現確認済み (切り分け済み)
- clippy / fmt clean

### 残課題

- `provider_settings_headless` 11 件の失敗は既存問題として未解決 (今回の変更とは独立と切り分け済み、別 intent で対応検討)
- models.dev provider ID ↔ evorch provider 名マッピングの open question は継続
