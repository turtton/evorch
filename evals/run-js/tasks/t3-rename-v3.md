# t3-rename-v3 — 複数 crate・多数の箇所の改名（一括置換 + 型を見て直す改名、コメントは除外）

| 項目 | 内容 |
|---|---|
| id | `t3-rename-v3` |
| 種類 | (3) 複数か所の編集 |
| 旧課題との違い | v2 は event-bus の 21 ファイル・4 識別子（単語置換で済む）。v3 は (A) event-bus の 3 識別子（コード 233 箇所・約 30 ファイル）に加え、(B) apply-patch の 4 つの型の**フィールド名**の改名（使用箇所は apply-patch の `src/`・`tests/` と tools の 7 ファイル・92 行）。(B) は同名の別のフィールド（`AppliedChange.raw` と `PatchTarget.raw` など）があり、単語置換ではコンパイルが通らない。型を見ながら、コンパイラのエラーに従って直す工程が 4〜5 回まわる |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t3-rename-v3.sh <clone>`（event-bus・apply-patch のテストと tools のテストのコンパイルを含む） |
| 想定ターン数 | 16〜30（`run_js` なしの素直な解き方。根拠は下） |

## プロンプト

```prompt
次の改名を行ってください。

(A) `crates/event-bus` の中（`src/` のコード・テスト、`tests/`。サブディレクトリを含む）で:
- 型 `SpanMapper` → `SpanTranslator`
- 型 `SpanDropKind` → `SpanDropReason`
- 関数・メソッド `start_run` → `begin_run`

(B) `crates/apply-patch` の公開型のフィールド名を変えます。使用箇所は `crates/apply-patch` の `src/`・`tests/` と、`crates/tools` にあります。
- `AppliedChange` のフィールド `path` → `final_path`（`raw` と `change` は変えない）
- `CommitFailure` のフィールド `committed` → `persisted`
- `ChangeKind` の各 variant のフィールド `old` → `before`、`new` → `after`
- `PatchTarget` のフィールド `raw` → `as_written`（`resolved` と `kind` は変えない。`AppliedChange` の `raw` は別のフィールドなので変えない）
同じ名前の別の型のフィールド（たとえば `Hunk` の `path` や `AppliedChange` の `raw`）を巻き込まないでください。ローカル変数や関数の引数の名前は、必要なら変えてかまいません。

ただし、行頭の空白を除いて `//` で始まる行（`//` `///` `//!` のコメント行）の中の言及は、ドキュメントを別途書き直すので今回は変更しないでください。コード行の末尾に付いたコメントは、その行ごとコードとして扱います。名前の一部だけが一致する別の識別子は変更しないでください。

変更してよいのは `crates/event-bus`、`crates/apply-patch`、`crates/tools` の `*.rs` だけです（`answers/` への書き込みは除きます）。最後に次の 3 つが通ることを確認してください。
- `cargo test -p event-bus --features otel-exporter --offline`
- `cargo test -p apply-patch --offline`
- `cargo check -p tools --tests --offline`（tools のテストは環境によって落ちるものがあるため、コンパイルが通れば十分です）

確認の質問はしないでください。曖昧な点は妥当な判断で進め、判断したことと置いた仮定を `answers/notes.md` に書いてください（`answers/` は作成済みです）。
```

## 想定解

- (A) 固定コミットの `crates/event-bus` のコード行 233 箇所（`SpanMapper` 85、`SpanDropKind` 89、`start_run` 68 のうち、コメント行の分を除く）を単語単位で置換する。参照解: `solutions/t3-rename-v3.sh`（コメント行を除く `sed` を `git grep -l | xargs` に渡す）。
- (B) 差分は 7 ファイル・92 行（`solutions/t3-rename-v3-b.patch`。コンパイルエラーに従って作った）。`cargo check --tests -p apply-patch -p tools` を繰り返すと、エラーが（`path` / `committed` / `old` / `new` の 3 つをまとめて直した場合）12 → 27 → 28 → 4 → 0 件、続けて `raw` が 40 → 1 → 0 件と進む（参照解を作ったときの実測）。パターンの短縮記法（`Updated { old, new }` → `Updated { before: old, after: new }`）や構造体リテラルの短縮記法（`PatchTarget { raw, resolved, kind }`）は、単純な置換では直らない。

落とし穴:
- **(B) は単語置換では通らない**: `\bpath\b` や `\braw\b` の一括置換は、`Hunk` や `AppliedChange.raw` など他の型の同名のフィールドを壊す。型を見て、コンパイルエラーの位置だけを直す必要がある。
- **短縮記法**: `AppliedChange { path, raw, change }` や `ChangeKind::Updated { old, new }` は、フィールド名とローカル変数名が同じ短縮形。フィールド名だけ変えると `final_path: path` / `before: old` の形になる（ローカル変数まで変えてもよい）。
- **コメント行の除外**: コメント行（日本語のドキュメントコメントなど）を一括置換すると、コンパイルもテストも通るので気づけない。`///` の rustdoc のリンク（[`SpanMapper`] など）が含まれる。
- `tools` のテストは環境依存で 1 件落ちる。`cargo test -p tools` ではなく `cargo check -p tools --tests` で確かめる。
- (A) の `start_run` は event-bus の内部のテストヘルパーでも使われている。`tests/` の統合テストの取りこぼしはコンパイルエラーになる。
- (B) の使用箇所が `crates/tools` にもあるので、`cargo test -p apply-patch` だけ通しても足りない。

## 想定ターン数の根拠

(A) は一括置換で 3〜6 リクエスト（洗い出し、置換、確認）。(B) はコンパイラ駆動で、`cargo check -p apply-patch -p tools --tests` を 5 回前後、エラーを見て直すことを繰り返す（1 周 2〜3 リクエスト）。最後に 3 つのコマンドでの確認と、見つかった取りこぼしの修正で 4〜6。全体で 16〜30 リクエスト（v2 の実測 5〜7 の約 4 倍）と見る。コンパイルに数十秒かかるため、時間は v2 の 3 倍ほど。

## 採点基準

- 機械採点（合否）: ①event-bus のコード行で旧名が 0 件、新名がファイル別に元の旧名と同数 ②apply-patch の 4 つの定義（`AppliedChange` に `final_path` と `raw`、`CommitFailure` に `persisted`、`ChangeKind` に `before` / `after`、`PatchTarget` に `as_written`、旧名なし）③event-bus / apply-patch / tools の全 `.rs` でコメント行が元と完全に同じ ④変更ファイルはこの 3 crate の中だけ ⑤上の 3 つのコマンドが成功。
- 追加観察: 編集の手段（`edit` の複数か所指定 / `apply_patch` / `sed -i` / `run_js`）、`edit` / `write` を通らずに shell が直接ファイルを書き換えた回数（ADR 0039 の観点）、(B) をコンパイラ駆動で直したか単語置換で壊したか、`cargo` を何回流したか、`run_js` の中で子呼び出しが多すぎて予算（`tool_call_threshold` 200、`max_file_rereads`）に当たらなかったか。
- LLM 採点の観点: 取りこぼしが後から見つかって手戻りしていないか、`answers/notes.md` に仮定が書かれているか。

## 検証コマンド

```sh
evals/run-js/verify/t3-rename-v3.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

(A) は約 30 ファイルに規則を、行の種類（コメント行かどうか）で条件を変えて適用する。`tools.grep` で洗い出し、`tools.read` で各ファイルを JS 側で受けて、行ごとに判定しながら正規表現で置換し、`tools.write` / `tools.edit` で書き戻せる。(B) は、`cargo check` の出力（エラー位置）を JS 側で解析して、位置ごとに直す往復を 1 回の呼び出しの中で回せる可能性がある（ただし ADR 0039 は、ビルドは通常の shell ジョブで実行するとしているため、使い分けを観察する）。
