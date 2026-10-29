# t6-debug-fix-v3 — 調査してから直す（3 crate・8 つの退行・新機能は残す）

| 項目 | 内容 |
|---|---|
| id | `t6-debug-fix-v3` |
| 種類 | (6) 調査してから直す |
| 旧課題との違い | v2 は apply-patch + agents の 5 退行・3 コミット。v3 は event-bus を加えた 3 crate・8 つの退行・4 コミット。コミット `apply-patch: add ChangeKind::label, …` は退行と新機能を同時に含み、コミット全体の revert では新機能が消える（検証が見る）。失敗は複数のテストバイナリ・crate に分かれ、テスト → 調査 → 修正 → 再テストを何度も回す |
| 準備 | `setup/t6-debug-fix-v3.sh`（`scripts/prepare-clone.sh` が自動で適用）。固定コミットの上に 4 つのコミットを積む（下表） |
| 検証 | `verify/t6-debug-fix-v3.sh <clone>`（3 crate の `cargo test` と隠しテストを含む） |
| 想定ターン数 | 20〜35 |

## プロンプト

```prompt
`cargo test -p apply-patch -p agents -p event-bus --features event-bus/otel-exporter --offline` が失敗するようになりました。原因を調べて、テストを変更せずに `crates/apply-patch/src/`、`crates/event-bus/src/`、`crates/agents/src/` のソースを修正し、すべて通る状態にしてください。

最近のコミットで入れた新機能 `ChangeKind::label()`（`crates/apply-patch/src/apply.rs`）と、そのテスト `crates/apply-patch/tests/label.rs` は残してください。コミット全体を巻き戻すと新機能も消えるので、退行だけを直してください。

修正が終わったら、原因を `answers/t6.md` に書いてください（`answers/` は作成済みです）。直したファイルごとに、何が壊れていたかを 1 行ずつ、ファイル名（`seek_sequence.rs` のような名前）を含めて書きます。

確認の質問はしないでください。曖昧な点は妥当な判断で進め、判断したことと置いた仮定を `answers/notes.md` に書いてください。
```

## 想定解

仕込んだ退行は 8 つ（パッチは `setup/t6-debug-fix-v3-{a,b,c,d}.patch`）:

| コミット | ファイル | 退行 | 失敗するテスト |
|---|---|---|---|
| a `apply-patch: tidy EOF search start and line-ending detection` | `apply-patch/src/seek_sequence.rs` | EOF 検索の開始位置 `eof_start.max(start)` が `min` | apply-patch の lib 2 件 |
| a | `apply-patch/src/text_file.rs` | 挿入行の改行を最初の改行で決めるのを最後の改行の上書きに | `tests/engine.rs` の 1 件 |
| b `event-bus: tidy ring fullness check and span admission bounds` | `event-bus/src/ring.rs` | `is_full` が `len() + 1 == capacity`（無害な `capacity()` の書き換えが混ざる） | event-bus の lib 2 件 |
| b | `event-bus/src/otel/span/span_budget.rs` | run ごとの上限の `>=` が `>`、属性数の上限の `<` が `<=` | event-bus の lib 2 件、`tests/otel_span_golden.rs` の 1 件 |
| c `apply-patch: add ChangeKind::label, simplify parent-dir check and summary buckets` | `apply-patch/src/targets.rs` | `..` の検査が `any(...)` から先頭の要素だけを見る `next().is_some_and(...)` に（無害な `collect::<PathBuf>()` が混ざる） | apply-patch の lib 1 件 |
| c | `apply-patch/src/apply.rs` | 削除の報告が deleted の枠ではなく modified の枠に入る（`reports[2]` → `reports[1]`）。**このコミットは新機能 `ChangeKind::label` と `tests/label.rs` も追加する** | `tests/engine.rs` の 2 件、`tests/upstream.rs` の 2 件 |
| d `agents: drop redundant lowercase oracle alias` | `agents/src/role.rs` | `from_name` から `"oracle"` の別名が消える | agents の `additional_roles` の 1 件 |

正しい修正は 7 ファイルの復元（`targets.rs` の `collect::<PathBuf>()` と `ring.rs` の `capacity()` は無害なので残してもよい。`apply.rs` は `label` を残す）。参照解: `solutions/t6-debug-fix-v3.sh`。

落とし穴:
- `cargo test` はテストバイナリ・crate ごとに止まるので、失敗が 1 つずつ現れる（`--no-fail-fast` を付ければ最初にまとめて見える）。直した後の再実行で、次の失敗が出る。
- `git log` / `git show` で 4 コミットを見れば原因が直接分かる。使うかどうかは観察対象で、採点では減点しない。ただしコミット c を丸ごと revert すると新機能 `label` が消え、検証が落ちる（`apply.rs` は `label` を残して `reports[1]` だけ直す）。
- `span_budget.rs` の 2 つの比較演算子（`>=` → `>`、`<` → `<=`）は別々の失敗として現れる。片方だけ直すと残りが 1 件落ち続ける。
- テストを緩める・消す修正は不合格（7 ファイルの `#[cfg(test)]` 以降の差分で検出する）。
- event-bus のテストは `--features otel-exporter` 付きでないと `tests/otel_span_golden.rs` の失敗が見えない。

## 想定ターン数の根拠

8 つの退行はテストバイナリ（apply-patch の lib / engine / upstream、event-bus の lib / golden、agents の additional_roles）に分かれて現れる。素直な解き方は、(1) 最初の `cargo test` で失敗を集める（`--no-fail-fast` の有無で 1〜3 回）、(2) 失敗したテストから対象を読む（テスト・ソース・`git show` で crate ごとに 3〜4 回）、(3) 編集して再テストを crate ごとに回す、(4) 取りこぼしを直す、(5) 報告で、20〜35 リクエスト（v2 の実測 7〜22 の約 2 倍）と見る。

## 採点基準

- 機械採点（合否）: ①変更が 7 ファイルと `tests/label.rs` に限られ、7 ファイルの `#[cfg(test)]` 以降が無改変 ②`cargo test -p apply-patch -p agents -p event-bus --features event-bus/otel-exporter --offline` が成功 ③隠しテストで `ChangeKind::label()` が 4 種とも残っている ④`answers/t6.md` に 7 ファイル名がすべて出てきて、7 行以上。
- 追加観察: 失敗の調べ方（テスト出力の読み方、`git show` / `git log` / `git diff` の使用）、全原因を特定するまでのターン数と `cargo test` の再実行の回数、テスト出力を丸ごとコンテキストに入れたか（`run_js` の中で `cargo test` を呼んで失敗行だけを返した場合は良い使い方か判断する）、`--no-fail-fast` を使ったか、新機能を残せたか。
- LLM 採点の観点: `answers/t6.md` の説明が実際の原因と合っているか、`answers/notes.md` に仮定が書かれているか。

## 検証コマンド

```sh
evals/run-js/verify/t6-debug-fix-v3.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

調査の段階（失敗したテストの特定、3 crate の定義・呼び出し元・テストの並行 read / grep、4 コミットの `git show`）が読み取りの連続になる。テスト出力は長いので、`run_js` で失敗行だけを抜き出して返せる。一方で `cargo test` 自体は shell ジョブとして流すほうが望ましいので、使い分けも観察できる。
