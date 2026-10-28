# t3-rename — 複数か所を同じ規則で変える編集

| 項目 | 内容 |
|---|---|
| id | `t3-rename` |
| 種類 | (3) 複数か所の編集 |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t3-rename.sh <clone>`（`cargo test -p apply-patch` を含む） |
| 想定ターン数 | 5〜12（`run_js` なしなら 8〜20。ファイルごとの `edit` か `sed -i` のどちらかに流れる） |

## プロンプト

```prompt
`crates/apply-patch` の中で、次の 2 つの識別子を改名してください。

- 構造体 `UpdateFileChunk` → `UpdateChunk`
- そのフィールド `is_end_of_file` → `at_eof`

`crates/apply-patch` 内のすべての使用箇所（`src/` のコード・テスト・doc コメント、`tests/`）を更新します。パッチ文法のマーカー文字列（`*** End of File` など）は変更しないでください。他の crate と `crates/apply-patch` 以外のファイルも変更しないでください。

最後に `cargo test -p apply-patch --offline` が通ることを確認してください。
```

## 想定解

- `UpdateFileChunk`: 31 行・5 ファイル（`src/lib.rs`、`parser.rs`、`streaming_parser.rs`、`file_update.rs`、`file_update_tests.rs`）→ `UpdateChunk`。
- `is_end_of_file`: 20 行・3 ファイル（`parser.rs`、`streaming_parser.rs`、`file_update.rs`）→ `at_eof`。
- 他の crate には使用箇所がない（`git grep` で確認済み）。変更ファイルは 5 つ以内。
- 参照解: `solutions/t3-rename.sh`（`git grep -l | xargs sed -E 's/\bX\b/Y/g'`）。

落とし穴:
- `UpdateFileChunk` は構造体リテラルや `..UpdateFileChunk::default()`、`use crate::UpdateFileChunk;`、`#[path]` で読み込まれる別ファイルのテスト（`file_update_tests.rs`）にもある。一部のファイルだけ直すとコンパイルが通らない。
- `at_eof` を grep で数えるときは `-w` が必要（`test_unified_diff_insert_at_eof` のような既存の識別子が部分一致する）。
- `*** End of File` を含む文字列やコメントは変更不可（`tool_contract.rs`、`parser.rs`、`streaming_parser.rs`、テストにある）。
- 改名で rustfmt の整形が変わりうるが、検証は fmt を見ない。

## 採点基準

- 機械採点（合否）: ①`UpdateFileChunk` / `is_end_of_file` が `crates/` に 0 件 ②`UpdateChunk` がちょうど 31 行、`at_eof` がちょうど 20 行（単語一致）③`End of File` の出現が `crates/apply-patch` で不変 ④変更が `crates/apply-patch/**/*.rs` の 5 ファイル以内 ⑤`cargo test -p apply-patch --offline` が成功。
- 追加観察: 編集の手段（`edit` の複数か所指定 / `apply_patch` / `sed -i` / `run_js`）、`edit` / `write` を通らずに shell が直接ファイルを書き換えた回数（ADR 0039 の観点）、改名後に `cargo test` を走らせたか。
- LLM 採点の観点: doc コメントやテストの取りこぼしが後から見つかって手戻りしていないか。

## 検証コマンド

```sh
evals/run-js/verify/t3-rename.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

使用箇所 51 行・5 ファイルに同じ規則を適用する。`tools.grep` で対象を洗い出し、`tools.read` で各ファイルの内容を JS 側で受けて正規表現（`\b` 付き）で置換し、`tools.write` / `tools.edit` で書き戻せる。編集が `edit` / `write` を通るため diff が残り、shell の `sed -i` と違って編集後フックも効く。ADR 0039 の「shell のスクリプトでファイルを書き換える」代替の観察対象。
