# t2-aggregate — ログや JSON の集計

| 項目 | 内容 |
|---|---|
| id | `t2-aggregate` |
| 種類 | (2) 集計 |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t2-aggregate.sh <clone>` |
| 想定ターン数 | 4〜10（`run_js` なしなら 6〜15。shell のスクリプトやワンライナーに流れやすい） |

## プロンプト

```prompt
`crates/` 以下の Rust ソースを crate ごとに集計して、結果を `answers/t2.json` に JSON で書いてください（`answers/` は作成済みです）。

対象は git で追跡されている `crates/<crate名>/` 以下の `*.rs` ファイルすべてです（`src/` も `tests/` も、サブディレクトリも含みます）。

1. `crates`: crate のディレクトリ名をキーとし、値を次の 3 つの数を持つオブジェクトにします。全 crate を含めます。
   - `rs_files`: `*.rs` ファイルの数
   - `rs_lines`: 改行文字（`\n`）の総数（各ファイルの `wc -l` の合計と同じ）
   - `test_attrs`: 行頭の空白を除いて `#[test]` で始まる行、または `#[tokio::test` で始まる行の数
2. `largest_files`: `rs_lines` と同じ数え方で行数が大きい順に上位 5 ファイル。`{"path": "リポジトリルートからの相対パス", "lines": 行数}` の配列です。

全体の形は `{"crates": {"agents": {"rs_files": 0, "rs_lines": 0, "test_attrs": 0}, ...}, "largest_files": [{"path": "...", "lines": 0}, ...]}` です。ソースコードは変更しないでください。
```

## 想定解

`verify/expected/t2-aggregate.json`（固定コミットから `scripts/gen-t2-expected.mjs` で算出。`rg` と `git ls-files` による別経路の突き合わせで一致を確認済み）。要点:

- crate は 22 個。`.rs` ファイルは計 1688 個。
- 大きい順に `runtime`（442 ファイル / 156,024 行 / test 1,973）、`gui`（444 / 113,821 / 1,702）、`providers`（196 / 43,395 / 607）、`storage`（132 / 43,810 / 478）、`tools`（117 / 30,933 / 509）。
- 上位 5 ファイル: `crates/runtime/tests/cache_preservation_e2e.rs`（4530）、`crates/runtime/src/runtime.rs`（4125）、`crates/event-bus/src/event.rs`（2929）、`crates/gui/src/runtime_sink.rs`（2833）、`crates/storage/src/writer/index/tests.rs`（2441）。6 位は 2034 行で、境界に同点はない。

落とし穴:
- `test_attrs` は `#[test]` と `#[tokio::test…]` を数える。`#[cfg(test)]` や `#[test_case]` は数えない。`#[tokio::test(flavor = …)]` のように引数付きも数える。
- 全ファイルを 1 つずつ読むと数十万行になる。`wc -l` / `rg -c` / `git ls-files` で集計するのが妥当で、ファイル本文をコンテキストに入れるのは誤用。
- `crates/` の外（`vendor/`、`target/` など）や未追跡ファイルを含めると数が合わない。

## 採点基準

- 機械採点（合否）: `crates` の 22 crate × 3 項目 + crate 数 + `largest_files` の件数と 5 件 × 2 項目の計 78 項目がすべて正解と一致。不一致の項目は検証が列挙する。
- 追加観察: 不一致の割合（部分点）、ソースを変更していないか、集計の手段（shell の `wc`/`rg`/`awk` の組み合わせか、スクリプトのヒアドキュメントか、`run_js` か）。
- LLM 採点の観点: 数え方の定義（改行数、行頭の空白除去）を守ったか。

## 検証コマンド

```sh
evals/run-js/verify/t2-aggregate.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

1,688 ファイルの集計で、中間結果（ファイル一覧、各ファイルの行数）は答えにまったく要らない。`tools.shell` や `tools.grep` で一覧と件数を取り、JS 内で crate ごとに集計して最終 JSON だけを `tools.write` で書ける。`run_js` なしでは shell のワンライナー（`;`/パイプ/`awk` の連結）か node/perl のヒアドキュメントに頼りやすく、ADR 0039 の計測の観点（複雑なワンライナー、スクリプトの利用）がそのまま観察できる。
