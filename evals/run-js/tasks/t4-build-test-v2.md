# t4-build-test-v2 — 調査 → 編集 → テスト → 修正の多段

| 項目 | 内容 |
|---|---|
| id | `t4-build-test-v2` |
| 種類 | (4) ビルド・テストを含む複数の手順 |
| 旧課題との違い | v1（`t4-build-test`、`Role::ALL` を足すだけ）に、権限の仕様変更（既存テストが 2 件落ちる → 期待値を直す）、逆引き API、doc 表の更新、ロール別の報告を足し、「変更 → テスト失敗 → 修正」の手戻りが自然に起きるようにした |
| 準備 | なし（固定コミットのまま）。事前に `cargo fetch` などで依存を取得済みにしておく |
| 検証 | `verify/t4-build-test-v2.sh <clone>`（fmt / clippy / test / 隠しテストを実行） |
| 想定ターン数 | 12〜25（ビルド・テストは通常の shell ジョブで流すのが望ましい） |

## プロンプト

```prompt
`crates/agents` のロール定義を次のとおり変更してください。ワークスペース全体のビルドは不要で、`crates/agents` の `src/` と `tests/` 以外は変更しません。依存の取得ができない環境では cargo に `--offline` を付けてください。

1. 変更前に `cargo test -p agents` を実行し、通ったテスト数の合計（すべてのテストバイナリと doc-test の `N passed` の合計）を控えます。
2. Explorer ロールに `git_diff` と `skill_load` を許可します（`Role::capabilities()`）。`crates/agents/src/role.rs` の doc コメントのロール一覧の表も合わせて直します。
3. 手順 2 で期待値が合わなくなる既存テストがあれば、新しい仕様に合わせて直します。テストを削除したり `#[ignore]` を付けたりしてはいけません。テストの数は減らさないでください。
4. `crates/agents/src/role.rs` の `impl Role` に次の 2 つを追加します。どちらも doc コメントを付けます。
   - `pub const ALL: [Role; 9]` — 全 variant を enum の宣言順に並べる
   - `pub fn roles_allowing(tool: &str) -> Vec<Role>` — `Role::capabilities().allowed_tools` に `tool` を含むロールを、宣言順に返す
5. 次を確かめるテストを、新規ファイル `crates/agents/tests/role_all.rs` に 3 件以上書きます。
   - `Role::ALL` の長さが 9 で、各要素が `Role::from_name(role.name())` で元に戻る
   - `Role::roles_allowing("skill_load")` が宣言順で期待どおり
   - `Role::roles_allowing("shell")` が Worker だけ、存在しないツール名では空
6. `cargo fmt -p agents -- --check`、`cargo clippy -p agents --all-targets -- -D warnings`、`cargo test -p agents` がすべて通る状態にします。
7. 結果を `answers/t4.json` に書きます（`answers/` は作成済みです）。形式は
   {"baseline_passed": <手順 1 の合計>, "final_passed": <手順 6 の最後の cargo test の合計>, "all_len": 9,
    "skill_load_roles": [<`skill_load` を許可するロール名を宣言順に>], "git_diff_roles": [<`git_diff` を許可するロール名を宣言順に>]}
   です。ロール名は実装した `roles_allowing` の結果と一致させてください。
```

## 想定解

- 手順 1 の合計は 28（`additional_roles` 6 件 + `capability_boundary` 22 件）。
- Explorer を `["read", "grep", "git_diff", "skill_load"]` にすると、`explorer_allows_exactly_adr_0002_tools`（`tests/support/capability_tools.rs` の `EXPLORER_TOOLS` に依存）と `explorer_denies_skill_load` の 2 件が落ちる。前者は定数に 2 ツールを足す、後者は「skill_load を許可する」テストに書き換える（名前も直すのが望ましい）。
- 最終は 28 + 新規 4 件 = 32（参照解）。`skill_load_roles` は Orchestrator, Explorer, Worker, Planner、`git_diff_roles` は Orchestrator, Explorer, Worker, Reviewer, Planner, Advisor。
- 参照解: `solutions/t4-build-test-v2.sh`。部分誤答は `solutions/t4-build-test-v2.wrong-*.sh`（壊れたテストを消す / 名前順に返す / 付与を忘れる）。

落とし穴:
- Explorer の変更後、テストの失敗は 1 回目の `cargo test` で初めて分かる（先に全部は見えない）。`cargo test` はバイナリごとに失敗で止まるので、`--no-fail-fast` を付けないと残りが見えない。
- 壊れたテストを消せば通るが、テスト数の下限（既存 28 件）で検出される。doc の表の更新を忘れやすい。
- clippy の `-D warnings` は `--all-targets` なので新規テストも対象になる。
- 合計の数え方（バイナリごとの `N passed` を足す）と、手順 1 と 6 の取り違え。
- `cargo test --workspace` を走らせると初回ビルドが長い（このタスクでは不要）。

## 採点基準

- 機械採点（合否）: ①変更が `crates/agents/{src,tests}/` に限られ、`role_all.rs` に `#[test]` が 3 件以上 ②既存の 2 つのテストファイルの `#[test]` が 28 件以上で `#[ignore]` なし ③`role.rs` の doc 表の Explorer の行に `git_diff` と `skill_load` ④`cargo fmt -- --check` ⑤`cargo clippy -D warnings` ⑥`cargo test -p agents` 成功で passed 合計が 28 + 新規テスト数以上 ⑦`answers/t4.json` の 5 項目が正解 ⑧隠しテスト（`Role::ALL` の順序、Explorer の権限、`roles_allowing` の順序と空の扱い）が成功。
- 追加観察: ビルド・テストの実行方法（通常の shell ジョブか、`run_js` の中か）、1 回の shell に連結した手順の数（build 連結数）、手順 1（変更前の実行）を省略していないか、失敗したテストの読み方と修正までのターン数。
- LLM 採点の観点: 書き換えたテストが空虚でないか（`explorer_allows_skill_load` が実際に Allowed を確かめているか）、doc コメントの質。

## 検証コマンド

```sh
evals/run-js/verify/t4-build-test-v2.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由（と効かない部分）

ロールの定義・既存テストの調査（`role.rs`、`tests/` 2 ファイルと `support/`）と、複数ファイルの編集は `run_js` でまとめられる。一方、中心の cargo の実行は ADR 0039 が「ビルドや長いテストは `run_js` の中で実行せず、通常の shell ジョブとして実行する」としているので、「向く部分と向かない部分が混ざる多段の作業で使い分けられるか」を見る。`run_js` の中で `cargo` を呼ぶ・ビルドと確認を 1 つのワンライナーに連結するといった誤用を観察する。
