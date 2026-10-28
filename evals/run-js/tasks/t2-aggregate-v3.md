# t2-aggregate-v3 — 多段の集計（Rust・Cargo.toml・git 履歴・ADR・拡張子表を合成）

| 項目 | 内容 |
|---|---|
| id | `t2-aggregate-v3` |
| 種類 | (2) 集計 |
| 旧課題との違い | v2 は Rust ファイルだけの集計。v3 は 6 つの異なる入力（Rust ソース、`Cargo.toml` の依存グラフ、`git log` の履歴、ADR の markdown、全追跡ファイルの拡張子表）を別々に集計して、最後に合成したランキングを作る。対象のファイル数は約 3 倍（Rust 1,688 に加え、約 2,950 の追跡ファイル全体を走査）。答えの項目は 399 |
| 準備 | なし（固定コミットのまま。`git log` を使うので履歴は clone に入っている） |
| 検証 | `verify/t2-aggregate-v3.sh <clone>` |
| 想定ターン数 | 16〜30（`run_js` なしの素直な解き方。根拠は下） |

## プロンプト

```prompt
このリポジトリの現在のコミットについて、次の 6 つを集計して、結果を `answers/t2.json` に JSON で書いてください（`answers/` は作成済みです）。「追跡されている」は git で追跡されている、の意味です。

### 1. `crates`
`crates/<crate名>/` 以下の追跡されている `*.rs`（サブディレクトリを含む）を、crate ごとに集計します。キーは crate のディレクトリ名で、全 crate を含めます。値は次の 6 つの数を持つオブジェクトです。
- `rs_files`: `*.rs` ファイルの数
- `rs_lines`: 改行文字（`\n`）の総数（各ファイルの `wc -l` の合計と同じ）
- `test_attrs`: 行頭の空白を除いて `#[test]` で始まる行、または `#[tokio::test` で始まる行の数
- `src_lines` / `tests_lines` / `other_lines`: `rs_lines` を、crate ディレクトリ直下の最初のディレクトリ名で分けたもの。`src/` 以下は `src_lines`、`tests/` 以下は `tests_lines`、それ以外（`benches/`、`examples/`、`build.rs` など）は `other_lines`

### 2. `graph`
各 crate の `crates/<crate名>/Cargo.toml` から、ワークスペース内の crate への依存（値に `path = "../<名前>"` を持つ項目）を調べます。`[dependencies]` と `[build-dependencies]` のものを通常の依存、`[dev-dependencies]` のものを dev の依存とします。キーは crate 名（`crates` と同じ全 crate）、値は次のオブジェクトです。
- `deps`: 通常の依存の crate 名を昇順に並べた配列
- `dev_deps`: dev の依存のうち、通常の依存に入っていないものを昇順に並べた配列
- `dependents`: その crate を通常の依存に持つ crate の数
- `transitive_deps`: 通常の依存をたどって到達できる crate の数（自分自身は数えない）
- `layer`: 通常の依存が無い crate は 0、あれば 1 + 依存先の `layer` の最大値

### 3. `history`
`4391a7e5` から遡った最新 200 コミット（`git log -200 4391a7e5`。パスで絞る前の 200 件）について、crate ごとに集計します。キーは `crates` と同じ全 crate、値は次のオブジェクトです。
- `commits_200`: その 200 コミットのうち、`crates/<crate名>/` 以下のファイルに触れたコミットの数
- `churn_200`: その 200 コミットでの `crates/<crate名>/` 以下の追加行数と削除行数の合計（`git log --numstat --no-renames` の数。バイナリ（`-`）は数えない）
- `last_date`: 触れたコミットのうち最も新しいもののコミット日付（`git log --format=%cs` の `YYYY-MM-DD`）。200 コミットの中に無ければ `null`

### 4. `adr`
`intents/evorch/decisions/` 直下の `NNNN-*.md`（4 桁の番号で始まる markdown。サブディレクトリの中は含めない）を ADR とします。
- `files`: ADR の数
- `status_counts`: 各 ADR の状態ごとの数。状態は、`## Status` という見出しの次の非空行が英字で始まるときのその最初の英単語を小文字にしたもの。見出しが無い、または次の非空行が英字で始まらないときは `none`。キーは状態名で昇順に並べる
- `most_referenced`: 他の ADR から最も参照されている上位 5 件。ADR の本文中の `ADR` の直後に（空白か `-` を挟んでもよい）続く数字列を、先頭の 0 を無視した ADR 番号の参照とみなす。ADR ごとに、その番号を（自分自身以外の）別の ADR ファイルが何ファイル参照しているかを数え（同じファイルが何回書いても 1）、対象は ADR ファイルが存在する番号だけにする。参照ファイル数の多い順、同数なら番号の小さい順。`{"adr": 番号(整数), "referenced_by": ファイル数}` の配列

### 5. `ext_matrix`
追跡されているすべてのファイルのうち、最上位ディレクトリが `crates` `.intent-cli` `intents` `docs` `vendor` `scripts` `examples` のものを、最上位ディレクトリ × 拡張子で集計します。拡張子は `rs` `md` `yaml` `json` `toml` `sql` のどれかなら（ファイル名の最後の `.` より後ろ）その名前、それ以外（拡張子なし・`png`・`txt` など）は `other`。キーは最上位ディレクトリ名、値は「該当のあった拡張子」をキーにした `{"files": ファイル数, "lines": 改行文字 (\n) の総数}`（該当の無い拡張子のキーは作らない）。

### 6. `hotspots`
`crates` の `rs_lines` が 3000 以上の crate だけを対象に、`score = commits_200 × (1 + dependents) ÷ (rs_lines ÷ 1000)` を小数第 2 位に四捨五入した値の大きい順に上位 5 crate。`{"crate": 名前, "score": 12.34, "commits_200": 数, "dependents": 数, "rs_lines": 数}` の配列です。

全体の形は `{"crates": {...}, "graph": {...}, "history": {...}, "adr": {...}, "ext_matrix": {...}, "hotspots": [...]}` です。ソースコードは変更しないでください（`answers/` への書き込みは除きます）。確認の質問はしないでください。曖昧な点は妥当な判断で進め、判断したことと置いた仮定を `answers/notes.md` に書いてください。
```

## 想定解

`verify/expected/t2-aggregate-v3.json`（`scripts/gen-v3-expected.py` で固定コミットから算出。agents の履歴、config / runtime / storage / tools の行数、event-bus の依存元、ADR 2 の被参照数は、`git log` / `wc` / `awk` / `grep` の別経路でも確認済み）。要点:

- `crates` は v2 と同じ値（22 crate、`.rs` 1,688 個）。
- `graph`: 葉（`deps` が空）は agents / apply-patch / browser / catalog / config / event-bus / mock-openai / secret-guard / workspace-ui の 9 個（browser と mock-openai の内部依存は dev だけ）。最も深いのは gui（layer 7、transitive_deps 18）。event-bus は依存元が 10。
- `history`: runtime 106、gui 113、tools 41、event-bus 25、agents 13 コミット。履歴の 200 コミットに触れない crate が 1 つある（`commits_200` 0、`last_date` null）。
- `adr`: 44 ファイル（サブディレクトリの `0037-…/` の中の markdown は数えない）。状態は accepted 40、proposed 1、none 3。被参照上位は ADR 2（9）、8（7）、27（7）、10（5）、12（5）。
- `ext_matrix`: crates の rs は 1,688 ファイル / 480,120 行。`png` などは `other`。
- `hotspots`: event-bus（21.63）、sandbox（13.31）、config（11.80）、routing（9.14）、browser（8.92）。

落とし穴（観察されたものを含む）:
- **`test_attrs` の正規表現**: `#[tokio::test…]` の `::` を `.` と書き間違えると tokio のテストを数えず、tools / providers / runtime などで大きく少なくなる（第 2 回の観察で gpt-6-luna+fast がこれで 23 項目を落とした）。`#[tokio::test(flavor = …)]` のように引数付きも数える。
- **`git log -200 -- crates/<crate>` は別の値になる**: パスで絞ってから 200 件を取るので、agents は 28 コミット（正しくは 13）になる。「全体の 200 コミットのうち、その crate に触れたもの」を数える。
- `churn_200` は `--no-renames` を付けないと名前変更が `old => new` の表記になり、パスの切り出しがずれる。
- `dev_deps` は通常の依存を除く。`layer` と `transitive_deps` は通常の依存だけでたどる（dev の依存を混ぜない）。
- ADR は `NNNN-*.md` だけ。`0037-owner-checkpoint-metadata-evidence/` の中の `README.md` などと `*.rs` は数えない（`crates` の `*.rs` にも含まれない）。`## Status` の次の行が日本語や数字で始まる ADR は `none`。
- `ext_matrix` の `lines` は、`png` などのバイナリも含めて `\n` バイトの総数。`other` は拡張子なし・`png`・`txt` などの合計。
- 全ファイルを読むと数十万行になる。`wc -l` / `git ls-files` / `git log --numstat` で集計するのが妥当で、本文をコンテキストに入れるのは誤用。

## 想定ターン数の根拠

6 つの入力はそれぞれ別の形式（Rust のテキスト、TOML、git の出力、markdown、ファイル一覧）で、パースの書き方を一から決めて試す必要がある。それぞれ「書く → 動かす → 出力を確かめて直す」を 2〜4 リクエスト、依存グラフの `layer` / `transitive_deps` と ADR の被参照は途中の出力を見て定義を確かめるのでやや多い。全体で 16〜30 リクエスト（v2 の実測 6〜8 の約 3 倍）と見る。

## 採点基準

- 機械採点（合否）: `crates` 22 × 6 + `graph` 22 × 5 + `history` 22 × 3 + `adr` 3 + `ext_matrix`（最上位ディレクトリごとの拡張子の集合、各セルの `files` と `lines`）+ `hotspots` の 5 件 × 5 項目、を含む 399 項目がすべて正解と一致（`score` は ±0.01 まで許容）。不一致は検証が列挙する。
- 追加観察: 不一致の割合（部分点）、ソースを変更していないか、集計の手段（`wc`/`awk`/`xargs` の連結、ヒアドキュメントのスクリプト、`run_js`）、`git log` の 200 の意味を取り違えていないか。
- LLM 採点の観点: 数え方の定義（改行数、行頭の空白除去、先頭ディレクトリでの分類）を守ったか、`answers/notes.md` に仮定が書かれているか。

## 検証コマンド

```sh
evals/run-js/verify/t2-aggregate-v3.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

6 つの集計はそれぞれ中間結果（ファイル一覧、各ファイルの行数、`git log` の長い出力、各 ADR の本文）が答えに要らない。`tools.shell` で一覧と履歴を取り、JS の中で crate × 指標の表、依存グラフの閉包、ADR の相互参照、拡張子の表、最後の合成ランキングまで組み立て、最終の JSON だけを `tools.write` で書ける。`run_js` なしでは shell のワンライナーの連結か python / node のヒアドキュメントに流れやすい。
