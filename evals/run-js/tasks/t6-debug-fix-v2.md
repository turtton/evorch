# t6-debug-fix-v2 — 調査してから直す（複数 crate・5 つの退行）

| 項目 | 内容 |
|---|---|
| id | `t6-debug-fix-v2` |
| 種類 | (6) 調査してから直す |
| 旧課題との違い | v1（`t6-debug-fix`、apply-patch の 2 退行）を、3 コミットに分けた 5 つの退行（apply-patch に 4、agents に 1）に広げた。失敗が別のテストバイナリに順に現れ、「テスト → 原因調査 → 修正 → 再テスト」を何度か回す |
| 準備 | `setup/t6-debug-fix-v2.sh`（`scripts/prepare-clone.sh` が自動で適用）。固定コミットの上に 3 つのコミットを積む（下表） |
| 検証 | `verify/t6-debug-fix-v2.sh <clone>`（`cargo test -p apply-patch -p agents` を含む） |
| 想定ターン数 | 12〜25 |

## プロンプト

```prompt
`cargo test -p apply-patch -p agents --offline` が失敗するようになりました。原因を調べて、テストを変更せずに `crates/apply-patch/src/` と `crates/agents/src/` のソースを修正し、すべて通る状態にしてください。

修正が終わったら、原因を `answers/t6.md` に書いてください（`answers/` は作成済みです）。直したファイルごとに、何が壊れていたかを 1 行ずつ、ファイル名（`seek_sequence.rs` のような名前）を含めて書きます。
```

## 想定解

仕込んだ退行は 5 つ（パッチは `setup/t6-debug-fix-v2-{a,b,c}.patch`）:

| コミット | ファイル | 退行 | 失敗するテスト |
|---|---|---|---|
| a `apply-patch: tidy EOF search start and line-ending detection` | `apply-patch/src/seek_sequence.rs` | EOF 検索の開始位置 `eof_start.max(start)` が `min` | lib の 2 件 |
| a | `apply-patch/src/text_file.rs` | 挿入行の改行を最初の改行で決めるのを最後の改行の上書きに | `tests/engine.rs` の 1 件 |
| b `apply-patch: simplify parent-dir check and summary buckets` | `apply-patch/src/targets.rs` | `..` の検査が `any(...)` から先頭の要素だけを見る `next().is_some_and(...)` に（同コミットに無害な `collect::<PathBuf>()` が混ざる） | lib の `targets_reject_parent_dir_components` |
| b | `apply-patch/src/apply.rs` | 削除の報告が deleted の枠ではなく modified の枠に入る（`reports[2]` → `reports[1]`） | `tests/engine.rs` の 2 件、`tests/upstream.rs` の 2 件 |
| c `agents: drop redundant lowercase oracle alias` | `agents/src/role.rs` | `from_name` から `"oracle"` の別名が消える | `agents` の `additional_roles` の 1 件 |

正しい修正は 5 ファイルの復元（`targets.rs` の `collect::<PathBuf>()` は無害なので残してもよい）。参照解: `solutions/t6-debug-fix-v2.sh`。

落とし穴:
- `cargo test` はテストバイナリごとに止まるので、失敗が lib → engine → upstream → agents の順に 1 つずつ現れる（`--no-fail-fast` を付ければ最初にまとめて見える）。直した後の再実行で、次の失敗が出る。
- `git log` / `git show` で 3 コミットを見れば原因が直接分かる。使うかどうかは観察対象で、採点では減点しない（実際の退行調査と同じ手段のため）。ただし 1 つのコミットに無害な変更が混ざっているので、コミット全体の巻き戻しだと無害な変更も消える（それでも合格）。
- テストを緩める・消す修正は不合格（5 ファイルの `#[cfg(test)]` 以降の差分で検出する）。

## 採点基準

- 機械採点（合否）: ①変更が 5 ファイルに限られ、各ファイルの `#[cfg(test)]` 以降が無改変 ②`cargo test -p apply-patch -p agents --offline` が成功 ③`answers/t6.md` に 5 ファイル名がすべて出てきて、5 行以上。
- 追加観察: 失敗の調べ方（テスト出力の読み方、`git show` / `git log` / `git diff` の使用）、全原因を特定するまでのターン数と `cargo test` の再実行の回数、テスト出力を丸ごとコンテキストに入れたか（`run_js` の中で `cargo test` を呼んで失敗行だけを返した場合は良い使い方か判断する）、`--no-fail-fast` を使ったか。
- LLM 採点の観点: `answers/t6.md` の説明が実際の原因と合っているか。

## 検証コマンド

```sh
evals/run-js/verify/t6-debug-fix-v2.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

調査の段階（失敗したテストの特定、`seek_sequence` / `targets` / `apply` の定義・呼び出し元・テストの並行 read / grep、3 コミットの `git show`）が読み取りの連続になる。テスト出力は長いので、`run_js` で失敗行だけを抜き出して返せる。一方で `cargo test` 自体は shell ジョブとして流すほうが望ましいので、使い分けも観察できる。
