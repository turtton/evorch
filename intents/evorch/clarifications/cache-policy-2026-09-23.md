# Cache-first: 出力境界と実測指標（2026-09-23）

## 背景・観測

[ADR 0003](../decisions/0003-cache-first-context-engine.md) の append-only 要件と、
大きなツール出力をプレビューとファイル参照へ制限する機能の整合を修復する。
2026-09-23 の thread-32 では、ツール結果が9件になった時点から古い結果が毎ターン
書き換えられ、キャッシュ再利用が低下した。提示された要求は provider 実測で
input=11,702、cache_read=1,792（15.31%）。旧警告の18.14%は実測の入力数ではなく
UTF-8 bytes / 4 による概算9,881を分母にしていた。

これは既存 append-only 要件からの実装逸脱と、診断の指標定義の不備である。
[2026-09-17 の補足](cache-policy-2026-09-17.md) の指標・warm判定の記述は本補足で更新する。
番号付き ADR、未実装の refresh / cache lease / provider 固有 compaction 要件は維持する。

## ツール結果の不変条件

- サイズ・行数制限、プレビュー作成、ファイル退避はツール結果の返却時に行う。
  同じ制限済みの結果をイベント、GUI、モデル履歴へ渡す。
- 送信済みのツール結果は、経過ターン数や後続結果の件数を理由に書き換えない。
  一度返したプレビューと artifact 参照もそのまま保持する。
- 履歴削減は明示的な手動・閾値 compaction の境界で行う。毎ループの
  古い結果置換を compaction の代替として走らせない。
- 一時 artifact の容量上限・保持期限は維持する。期限切れによって過去メッセージを
  書き換えず、参照先が期限切れの場合は読み取り時に報告する。

## 実測率と回帰の区別

- `cache_hit_ratio = cache_read_tokens / input_tokens`。input は各providerのcache read/writeを
  含む正規化済み総入力。GUIの率も同じ分母を使う。入力0・cache小計が総入力を超えるなど
  不正なusageは未知として扱い、率や回帰を捏造しない。
- `expected_cacheable_tokens` と `utf8_bytes_div_4` をキャッシュ監視から廃止する。
  トークナイザー、サーバー配置、失効状態を知らずに「再利用可能」と断定しない。
  コンテキストの容量見積もりは別用途であり、本変更の対象ではない。
- 比較対象は同じ bus/run/provider/profile/protocol/model の直近完了要求。
  実際の wire 入力と設定のハッシュで、その要求の全入力が現在の先頭に残ることを確認する。
  本文・画像・ツール引数は観測用ストアに保持せず、固定サイズのハッシュを使う。
- 非送信のReasoningは比較に含まれない。送信されるReasoningは各wire形式に従う。
  Anthropicの末尾 `cache_control` 移動とstream輸送設定だけは入力変更から除外する。
  ツール引数・schemaの同名キーは除外しない。
- `previous_cache_tokens` は比較対象の実測cache read+write。
  `cache_retention_ratio = 現在のcache_read / previous_cache_tokens` が0.5未満なら
  `CacheRegression`。警告に実測hit率、比較対象request ID、両方のtoken数・率を明示する。
  この比較率は新たなキャッシュ再利用が加わると1を超えうる。
- cold要求、観測なし、入力変更・compaction、基準のcache量0、前回要求開始から5分以上
  経過した場合は比較しない。5分は保守的な診断窓であり、providerのTTL保証ではない。
  並行cold要求を後からwarm扱いせず、古い要求の遅延完了で新しい基準を上書きしない。
- 現在の入力が新規suffixで増え、実測hit率だけが下がっても、以前のcache量を維持して
  いれば回帰とは判定しない。部分prefixの厳密な再利用可能token数や個々のミス理由は
  provider診断なしには判定しない。

## 検証

- runtime: 10件の大きなtool resultを実executorへ返し、11回のモデル要求の既送信prefixが
  不変、イベントとモデルの返却内容が一致、artifactの全文が読み戻せることを確認する。
- Codex wiremock/SSE: 1,792/11,702の実測率、非送信Reasoning、古い出力変更、長いsuffixを確認。
- observer: cache write、cold/並行要求、scope分離、期限、遅延完了、欠落/不正usageを確認。
- 通常のcompactionの既存回帰テストを維持する。

実装: `crates/runtime/src/agent_loop.rs`、`crates/tools/src/output.rs`、
`crates/providers/src/observe/cache.rs`、`cache_prefix.rs`。
