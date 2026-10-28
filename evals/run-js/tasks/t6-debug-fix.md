# t6-debug-fix — 調査してから直す、混ざった作業

| 項目 | 内容 |
|---|---|
| id | `t6-debug-fix` |
| 種類 | (6) 調査してから直す |
| 準備 | `setup/t6-debug-fix.sh`（`scripts/prepare-clone.sh` が自動で適用）。固定コミットの上に「`apply-patch: tidy EOF search start and line-ending detection`」というコミットを 1 つ積み、`crates/apply-patch` に 2 つの退行を入れる |
| 検証 | `verify/t6-debug-fix.sh <clone>`（`cargo test -p apply-patch` を含む） |
| 想定ターン数 | 6〜14（`run_js` なしなら 8〜18） |

## プロンプト

```prompt
`cargo test -p apply-patch --offline` が失敗するようになりました。原因を調べて、テストを変更せずに `crates/apply-patch/src/` のソースを修正し、すべて通る状態にしてください。

修正が終わったら、原因を `answers/t6.md` に書いてください（`answers/` は作成済みです）。直したファイルごとに、何が壊れていたかを 1 行ずつ、ファイル名を含めて書きます。
```

## 想定解

仕込んだ退行は 2 つ（パッチは `setup/t6-debug-fix.patch`、いずれも 1 行）:

1. `crates/apply-patch/src/seek_sequence.rs`: EOF 検索の開始位置 `eof_start.max(start)` が `eof_start.min(start)` になっている。失敗するテスト: `seek_sequence::tests::test_empty_pattern_and_start_bounds`、`seek_sequence::tests::test_eof_search_only_matches_the_file_end`（lib）。
2. `crates/apply-patch/src/text_file.rs`: 挿入行に使う改行を「最初の改行」にする `preferred_ending.get_or_insert(ending)` が、最後の改行で上書きする `preferred_ending = Some(ending)` になっている。失敗するテスト: `matching_and_line_endings_are_upstream_compatible`（tests/engine.rs）。

正しい修正は 2 行の復元（固定コミットのソースと一致）。参照解: `solutions/t6-debug-fix.sh`。

落とし穴:
- 失敗が 2 つの独立した原因から来ており、片方だけ直しても全体は通らない（3 件の失敗の原因が別）。
- `git show HEAD` で退行コミットを見れば原因が直接分かる。使うかどうかは観察対象で、採点では減点しない（実際の退行調査と同じ手段のため）。
- テストを緩める・消す修正は不合格（`seek_sequence.rs` のテスト部の差分で検出する）。

## 採点基準

- 機械採点（合否）: ①変更が `seek_sequence.rs` と `text_file.rs` に限られ、`seek_sequence.rs` の `#[cfg(test)]` 以降が無改変 ②`cargo test -p apply-patch --offline` が成功 ③`answers/t6.md` に `seek_sequence` と `text_file` の両方が出てくる。
- 追加観察: 失敗の調べ方（テスト出力の読み方、`git show HEAD` / `git log` / `git diff` の使用）、原因ファイルを特定するまでのターン数、再実行の回数、テスト出力を丸ごとコンテキストに入れたか（`run_js` の中で `cargo test` を呼んで失敗行だけを返した場合は良い使い方か判断する）。
- LLM 採点の観点: `answers/t6.md` の説明が実際の原因と合っているか。

## 検証コマンド

```sh
evals/run-js/verify/t6-debug-fix.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

調査の段階（失敗したテストの特定、`seek_sequence` の定義・呼び出し元・テストの並行 read / grep）が読み取りの連続になる。テスト出力は長いので、`run_js` で失敗行だけを抜き出して返せる。一方で `cargo test` 自体は shell ジョブとして流すほうが望ましいので、使い分けも観察できる。
