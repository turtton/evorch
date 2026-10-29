# t4-build-test — ビルドやテストを含む複数の手順

| 項目 | 内容 |
|---|---|
| id | `t4-build-test` |
| 種類 | (4) ビルド・テストを含む複数の手順 |
| 準備 | なし（固定コミットのまま）。事前に `cargo fetch` などで依存を取得済みにしておく |
| 検証 | `verify/t4-build-test.sh <clone>`（fmt / clippy / test / 隠しテストを実行） |
| 想定ターン数 | 8〜16（`run_js` なしでも同程度。ビルド・テストは通常の shell ジョブで流すのが望ましい） |

## プロンプト

```prompt
`crates/agents` に、全ロールを列挙する公開定数を追加してください。次の手順で進めます。

1. まず現状のテストを `cargo test -p agents` で実行し、通ったテスト数の合計（すべてのテストバイナリと doc-test の `N passed` の合計）を控えます。
2. `crates/agents/src/role.rs` の `impl Role` に `pub const ALL: [Role; 9]` を追加します。要素は `Role` の全 variant で、enum の宣言順です。doc コメントを付けます。
3. 次の性質を確かめるテストを、新規ファイル `crates/agents/tests/role_all.rs` に 2 件以上書きます。既存のテストは変更しません。
   - `Role::ALL` の長さが 9 である
   - `Role::ALL` の各要素が `Role::from_name(role.name())` で元に戻る
4. `cargo fmt -p agents -- --check`、`cargo clippy -p agents --all-targets -- -D warnings`、`cargo test -p agents` がすべて通る状態にします。ワークスペース全体のビルドは不要です。依存の取得ができない環境では `--offline` を付けてください。
5. 結果を `answers/t4.json` に書きます（`answers/` は作成済みです）。形式は `{"baseline_passed": <手順 1 の合計>, "final_passed": <手順 4 の最後の cargo test の合計>, "all_len": 9}` です。

`crates/agents/src/` と新規テストファイル以外は変更しないでください。
```

## 想定解

- 手順 1 の合計は 28（`additional_roles` 6 件 + `capability_boundary` 22 件。lib と doc-test は 0 件）。
- `role.rs` に `ALL` を 9 要素（Orchestrator, Explorer, Worker, Reviewer, WebResearcher, Planner, Advisor, MultimodalLooker, Evaluator）で追加し、`tests/role_all.rs` に 2 件以上のテスト。最終の合計は 28 + 追加件数。
- 参照解: `solutions/t4-build-test.sh`。

落とし穴:
- clippy の `-D warnings` は `--all-targets` なので新規テストも対象になる。
- 合計の数え方（バイナリごとの `N passed` を足す）と、手順 1 と 4 の取り違え。
- `cargo test --workspace` や `cargo build` を走らせると初回ビルドが長い（このタスクでは不要）。
- 長いビルドと確認を `;` / `&&` でつないだワンライナーにまとめるか（ADR 0039 の観察項目）。

## 採点基準

- 機械採点（合否）: ①変更が `crates/agents/src/` と新規 `crates/agents/tests/role_all.rs` だけ ②`role_all.rs` に `#[test]` が 2 件以上 ③`cargo fmt -p agents -- --check` 成功 ④`cargo clippy -p agents --all-targets -- -D warnings` 成功 ⑤`cargo test -p agents` 成功し、passed 合計が 28 + 新規テスト数以上 ⑥`answers/t4.json` の `baseline_passed` = 28、`final_passed` = 実際の合計、`all_len` = 9 ⑦隠しテスト（`Role::ALL` の順序、往復と一意性、`[Role; 9]` 型）が成功。
- 追加観察: ビルド・テストの実行方法（通常の shell ジョブか、`run_js` の中か）、1 回の shell に連結した手順の数、手順 1（変更前の実行）を省略していないか。
- LLM 採点の観点: doc コメントの質、テストが空虚でないか。

## 検証コマンド

```sh
evals/run-js/verify/t4-build-test.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由（と効かない部分）

`role.rs` の読み取りと既存テストの調査は `run_js` でまとめられるが、中心の cargo の実行は時間がかかりうる。ADR 0039 が「ビルドや長いテストは `run_js` の中で実行せず、通常の shell ジョブとして実行する」としているため、この課題は「向く部分と向かない部分が混ざる場面で使い分けられるか」を見るためのもの。`run_js` の中で `cargo` を呼ぶ・ビルドと確認を 1 つのワンライナーに連結するといった誤用を観察する。
