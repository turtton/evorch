# t3-rename-v2 — 複数ファイル・複数種の改名（一部を除外）

| 項目 | 内容 |
|---|---|
| id | `t3-rename-v2` |
| 種類 | (3) 複数か所の編集 |
| 旧課題との違い | v1（`t3-rename`、apply-patch の 5 ファイル 51 行）を、`crates/event-bus` の 21 ファイル・4 種の識別子・コード行の 317 箇所に広げ、コメント行は変えない条件と、部分一致で別の識別子（`str_attribute`）を壊さない罠を足した |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t3-rename-v2.sh <clone>`（`cargo test -p event-bus --features otel-exporter --offline` を含む） |
| 想定ターン数 | 10〜25（`run_js` なしなら 15〜35。ファイルごとの `edit` か `sed -i` に流れる） |

## プロンプト

```prompt
`crates/event-bus` の中で、次の 4 つの識別子を改名してください。
- 型 `SpanKey` → `SpanRef`
- 型 `SpanAction` → `SpanCommand`
- メソッド `drain_drops` → `take_drops`
- 関数 `str_attr` → `string_attr`

対象は `crates/event-bus` 内のすべての Rust ファイル（`src/` のコード・テスト、`tests/`）です。使用箇所は 20 ファイルほどに散らばっています。

ただし、行頭の空白を除いて `//` で始まる行（`//` `///` `//!` のコメント行）の中の言及は、ドキュメントを別途書き直すので今回は変更しないでください。コード行の末尾に付いたコメントは、その行ごとコードとして扱います。名前の一部だけが一致する別の識別子（たとえば `str_attribute`）は変更しないでください。

`crates/event-bus` 以外のファイルは変更しないでください。最後に `cargo test -p event-bus --features otel-exporter --offline` が通ることを確認してください。
```

## 想定解

固定コミットの `crates/event-bus` の 21 ファイル、コード行の 317 箇所（単語の出現数。行数ではコメント行を除いて `SpanKey` 140、`SpanAction` 96、`drain_drops` 37、`str_attr` 38）を単語単位で置換する。参照解: `solutions/t3-rename-v2.sh`（コメント行を除外する `sed` を `git grep -l | xargs` に渡す）。他の crate には使用箇所がない（`git grep` で確認済み）。

落とし穴:
- `str_attr` と `str_attribute` は部分一致する（`str_attribute` は 28 行、別の識別子）。`\b` なしの置換は `str_attribute` を壊す。`grep -w` も使わないと数が合わない。
- コメント行 6 箇所（`span/mod.rs` に 4、`span_exporter.rs` に 2）は変更しない。一括置換するとここが変わる（コンパイルもテストも通るので、テストだけでは気づけない）。
- `SpanKey` は `pub use` で再公開され、`src/otel/span_exporter.rs`（feature `otel-exporter` のときだけコンパイル）と `tests/otel_span_golden.rs` からも使われる。feature 無しでテストを走らせると取りこぼしに気づけない。
- 改名で rustfmt の整形が変わりうるが、検証は fmt を見ない。

## 採点基準

- 機械採点（合否）: ①コード行に旧名が 0 件、新名がファイル別に元の旧名と同数 ②コメント行（全ファイル）が元と完全に同じ ③`str_attribute` の個数が不変 ④変更されたファイルは改名対象のファイルだけで、`crates/event-bus` の外は変更なし ⑤`cargo test -p event-bus --features otel-exporter --offline` が成功。
- 追加観察: 編集の手段（`edit` の複数か所指定 / `apply_patch` / `sed -i` / `run_js`）、`edit` / `write` を通らずに shell が直接ファイルを書き換えた回数（ADR 0039 の観点）、コメント行の除外をどう実現したか（`sed` のアドレス指定、JS の行ごとの判定、手作業）、`str_attribute` の誤置換から手戻りがあったか。
- LLM 採点の観点: 取りこぼしが後から見つかって手戻りしていないか。

## 検証コマンド

```sh
evals/run-js/verify/t3-rename-v2.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

21 ファイルに 4 種の規則を、行の種類（コメント行かどうか）で条件を変えて適用する。`tools.grep` で対象を洗い出し、`tools.read` で各ファイルの内容を JS 側で受けて、行ごとに判定しながら正規表現（`\b` 付き）で置換し、`tools.write` / `tools.edit` で書き戻せる。編集が `edit` / `write` を通るので diff が残り、`sed -i` と違って編集後フックも効く。shell の `sed -i` に条件付きのアドレス指定を重ねる代替も観察対象。
