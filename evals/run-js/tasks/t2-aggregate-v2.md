# t2-aggregate-v2 — 集計の後に変換・ランキング・クロス集計

| 項目 | 内容 |
|---|---|
| id | `t2-aggregate-v2` |
| 種類 | (2) 集計 |
| 旧課題との違い | v1（`t2-aggregate`）の crate 別 3 指標に、`src` / `tests` / その他の行数のクロス集計、条件付きの密度ランキング（小数の計算）、ファイル名で crate をまたぐ集計を足した |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t2-aggregate-v2.sh <clone>` |
| 想定ターン数 | 8〜20（`run_js` なしなら 12〜30。shell のスクリプトやヒアドキュメントに流れやすい） |

## プロンプト

```prompt
`crates/` 以下の Rust ソースを集計して、結果を `answers/t2.json` に JSON で書いてください（`answers/` は作成済みです）。

対象は git で追跡されている `crates/<crate名>/` 以下の `*.rs` ファイルすべてです（サブディレクトリも含みます）。

1. `crates`: crate のディレクトリ名をキーとし、値を次の 6 つの数を持つオブジェクトにします。全 crate を含めます。
   - `rs_files`: `*.rs` ファイルの数
   - `rs_lines`: 改行文字（`\n`）の総数（各ファイルの `wc -l` の合計と同じ）
   - `test_attrs`: 行頭の空白を除いて `#[test]` で始まる行、または `#[tokio::test` で始まる行の数
   - `src_lines` / `tests_lines` / `other_lines`: `rs_lines` を、crate ディレクトリ直下の最初のディレクトリ名で分けたもの。`src/` 以下は `src_lines`、`tests/` 以下は `tests_lines`、それ以外（`benches/`、`examples/`、`build.rs` など）は `other_lines`。3 つの合計は `rs_lines` になります
2. `density_top5`: `rs_lines` が 8000 以上の crate だけを対象に、`test_attrs` ÷ `rs_lines` × 1000（1000 行あたりのテスト属性数）を小数第 2 位に四捨五入した値 `per_1k` の大きい順に上位 5 crate。`{"crate": "名前", "per_1k": 12.34}` の配列です。
3. `shared_basenames`: ファイル名（パスの最後の要素。例 `error.rs`）が 5 つ以上の異なる crate に存在するものを集めます。ただし `lib.rs` `mod.rs` `main.rs` `build.rs` は除きます。各ファイル名について `{"name": "error.rs", "crate_count": 異なる crate の数, "file_count": そのファイル名のファイルの総数, "total_lines": それらのファイルの `rs_lines` の合計}` とし、`total_lines` の大きい順に並べた配列にします。

全体の形は次のとおりです。ソースコードは変更しないでください。

{"crates": {"agents": {"rs_files": 0, "rs_lines": 0, "test_attrs": 0, "src_lines": 0, "tests_lines": 0, "other_lines": 0}, ...},
 "density_top5": [{"crate": "...", "per_1k": 0.0}, ...],
 "shared_basenames": [{"name": "...", "crate_count": 0, "file_count": 0, "total_lines": 0}, ...]}
```

## 想定解

`verify/expected/t2-aggregate-v2.json`（`scripts/gen-v2-expected.mjs` で固定コミットから算出。`git ls-files` + `wc -l` + `grep -c` による別経路の突き合わせで、config / runtime / storage / tools の 6 指標と shared_basenames の候補を確認済み）。要点:

- crate は 22 個、`.rs` は 1688 個。`other_lines` が 0 でないのは config(16)、runtime(2922)、storage(1344) だけ。
- `density_top5`: config 21.52、tools 16.45、gui 14.95、providers 13.99、event-bus 13.77（8000 行以上の crate は 8 個）。5 位と 6 位（runtime 12.65）は離れていて、四捨五入で順位は入れ替わらない。
- `shared_basenames`: `tests.rs`(11 crate / 62 ファイル / 30,085 行)、`error.rs`(9 / 12 / 1,493)、`types.rs`(5 / 6 / 1,115)、`session.rs`(5 / 6 / 971)、`ownership.rs`(5 / 7 / 510)。`lib.rs`(22 crate)と `mod.rs`(8 crate)は除外の対象。

落とし穴:
- 3 つの行数を `src/` `tests/` の先頭ディレクトリで分けるので、`src/…/tests.rs` は `tests_lines` ではなく `src_lines`。`tests/` 以下のサブディレクトリ（`tests/support/` など）は `tests_lines`。
- ファイル名の集計は crate をまたぐ。ディレクトリ名ではなくパスの最後の要素。
- 密度は 8000 行未満の crate を先に除く。除かないと sandbox(18.63)などが混ざる。
- 全ファイルを読むと数十万行になる。`wc -l` / `grep -c` / `git ls-files` で集計するのが妥当で、ファイル本文をコンテキストに入れるのは誤用。

## 採点基準

- 機械採点（合否）: `crates` の 22 crate × 6 項目 + crate 数 + `density_top5` の件数と 5 件 × 2 項目（`per_1k` は ±0.01 まで許容）+ `shared_basenames` の件数と 5 件 × 4 項目、の全項目が正解と一致。不一致は検証が列挙する。
- 追加観察: 不一致の割合（部分点）、ソースを変更していないか、集計の手段（`wc`/`awk`/`xargs` の連結、ヒアドキュメントのスクリプト、`run_js`）。
- LLM 採点の観点: 数え方の定義（改行数、行頭の空白除去、先頭ディレクトリでの分類）を守ったか。

## 検証コマンド

```sh
evals/run-js/verify/t2-aggregate-v2.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

1,688 ファイルの集計で、中間結果（ファイル一覧、各ファイルの行数）は答えに要らない。さらに v2 は集計後の加工（分類、除外、比率、ソート、ファイル名のグルーピング）が続く。`tools.shell` で `git ls-files` と行数を取り、JS 内で crate × 分類の表を作り、比率と順位付け、グルーピングまで済ませて最終の JSON だけを `tools.write` で書ける。`run_js` なしでは shell のワンライナーの連結か python/node のヒアドキュメントに頼りやすい。
