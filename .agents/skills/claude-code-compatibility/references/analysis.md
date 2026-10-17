# 解析と検証

この資料は解析方法を扱う。現在の定数、対応モデル、意図した native との差分は
[互換性ドキュメント](../../../../docs/claude-compatibility.md) と実装で確認する。

## 対象の固定

調査メモに次を記録する。secret や account ID は含めない。

- 実行 wrapper と native binary の実体パス、binary SHA-256、隔離環境で確認した CLI version/build、OS/arch。
- SDK/runtime の値と、その出所が静的解析か通信実測か。
- OMP repository URL と full commit、evorch の HEAD。
- 採取 mode、model/settings、起こした操作、比較対象となる request 種別。

既存 fixture の provenance に binary hash/arch 等がなければ「未記録」とする。
現在の手元の binary の値を過去の採取へ遡って当てはめず、次の再採取で証拠を揃える。

`command -v claude` と `readlink -f` だけで native の実体が分かるとは限らない。
Nix の wrapper や sandbox launcher を確認し、wrapper を外して起動するなら失われる
環境・制約を把握する。version を確認する起動も [隔離採取](capture.md) の環境で行う。
過去の `/nix/store` パスや `/tmp` の資料を前提にせず、必要な入力をその都度用意する。

OMP は指定した commit の `git show` または隔離 checkout を読む。
`git rev-parse HEAD` と記録した commit を突き合わせ、一時 clone の HEAD を参照 commit
と取り違えない。方式の参考にしても、native の新しい実測より古い定数を優先しない。

## 静的解析の入口

binary 内の埋込み JS は意味のある文字列から位置を探し直す。最初は一致語と offset
だけを出し、必要な呼出経路を小さい範囲で読む。全体を stdout に dump しない。

```sh
rg --text --only-matching --byte-offset \
  'x-anthropic-billing-header|cc_version|cc_entrypoint|cc_prompt_id|x-claude-code-request-class|x-claude-code-parent-agent-id|X-Stainless-Package-Version' \
  "$analysis_native_binary"
```

検索語は手掛かりであり、新 version で見つからない場合は構造から探す。
旧 offset・minified symbol・以前の CLI flag の効果をそのまま当てはめない。
単一関数だけで判断せず、値の生成元、caller、mode 分岐、最終 request への挿入箇所を追う。

| 調べる面 | 必要な証拠 | evorch の主な確認先 |
| --- | --- | --- |
| CLI/SDK/runtime、beta、reasoning | 定数の出所と最終送信値、field を送る条件 | `crates/providers/src/provider/claude/profile.rs` |
| body の順序、billing、checksum | 最終 serialized bytes と変換の順序 | 同 `request.rs`、`fingerprint.rs` |
| tool 名・SSE 復元 | 定義、履歴、fragmented response の変換 | 同 `tool_adapter.rs`、`crates/providers/src/wire/anthropic/` |
| installation/account/session | seed の寿命、token と metadata の対応 | 同 `identity.rs`、`mod.rs`、`crates/routing/src/factory.rs` |
| prompt/attempt/response/agent ID | caller の mode・目的・階層と成功時の更新条件 | 同 `transport_context.rs`、`crates/runtime/src/request_turn.rs` |
| HTTP/TLS、圧縮、CA、retry | raw wire、runtime の client 設定 | 同 `transport.rs`、`crates/providers/src/provider/anthropic/transport.rs` |

UI の main、print/SDK、auxiliary、child、compaction の分岐を区別する。
階層の意味は生成元まで追い、root の run/session ID を parent subagent ID と推測しない。
実測で確かめる対象は [capture matrix](capture.md) に従う。

## Checksum を独立に確かめる

1. 対象 version の suffix sample、文字コード、salt/hash/seed、切り出し、除外処理、placeholder の挿入順を静的解析する。値を旧 skill から転記しない。
2. 既知の synthetic user text を使った native request を隔離 mock で受け、**最終 serialized bytes** のまま検算する。解析用 JSON の parse/re-serialize を oracle にしない。
3. adapter が生成した対象 billing block の checksum placeholder だけを復元する。user text、schema、引数、過去 tool result の同じ文字列を置換しない。
4. native の式から別実装で synthetic vector を作り、Rust の production 関数を期待値生成に使わない。独立 runtime、例えば確認した Bun の hash API、を使った場合は version と方法を記録する。
5. ASCII、短い/空の入力、非 BMP 文字と片側 surrogate、空白・escaping、入れ子の routing literal、sentinel の重複を必要な差分に応じて検証する。除外した値が wire 自体から消えていないことも見る。

既存の独立 vector は `crates/providers/src/provider/claude/fingerprint/tests.rs`、
HTTP 境界の独立 decoder は `crates/mock-openai/src/claude_contract.rs` にある。
新しい golden を production 出力へ合わせるだけでは独立検証にならない。

## 採否と検証

差分ごとに「native 実測／静的解析／OMP からの推定／未確認」、採用するか、理由と
影響範囲を残す。evorch に測定値や対応機能がない場合、native 風の metadata/beta を
作り足さない。意図した session mapping 等の差分は現在のドキュメントで確認する。

production を更新する場合は、互換性ドキュメントの更新 gate と現在の CI に従う。
変更面に対応する suite の例は次のとおり。target 名は実行前に checkout で確認する。

- provider: `claude_contract`、`claude_attempt_metadata`、`claude_account_binding`、`claude_transport`、`anthropic_signed_replay` と `provider::claude` の unit tests。
- runtime: `claude_cache_e2e`、`claude_compaction_cache`、`request_turn_context`。API/OAuth 両方の tool loop と、実 compaction 後の新 prefix の再利用を見る。
- cache: `scripts/check-cache-contracts.sh`。入力由来の mock cache と独立 wire-prefix assertions、正しい checksum を持つ負例を維持する。
- build 依存や transport を変えた場合: workspace Clippy/format、Nix package build と installCheck。Linux では環境変数に頼らない CLI 起動と ELF runtime 依存も確認する。

通常ターンの送信済み system/tools/settings/order/history/cache affinity を維持し、
明示された compaction/model switch の一度の置換境界だけを認める。
通すために一般の prefix checker や負例を緩めない。
一時 Nix store の toolchain パスを保存せず、現在の `rust-toolchain.toml`、`flake.nix` と
プロジェクトの開発環境を使う。

skill/helper だけの変更では、Rust 全体の再実行を自動的な必須条件にしない。
helper の focused test、独立 fixture との比較、frontmatter と相対リンクを検証する。
判断手順を変更したら、履歴を持たない独立 agent に skill と小さな synthetic task を渡し、
offline で試用させる。期待する答えを先に渡さず、実際の判断・成果物をレビューする。

## 報告と次回への反映

報告には pin 情報、観測条件、差分と採否、独立 vector/fixture の由来、実行した検証、
未測定の面と理由を含める。secret や native の bundled prompt/tool 本文は含めない。

今回の新事実から方法を一般化できる場合、同じ作業で skill/reference/helper を修正する。
既知の罠が解消されたなら旧手順を削除・統合し、単なる version 履歴を本文へ積み増さない。
現行仕様の変更は互換性ドキュメントと production に置き、ここへ二重管理しない。
