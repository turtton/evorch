# t4-build-test-v3 — 3 crate にまたがる、調査 → 編集 → テスト → 修正の多段

| 項目 | 内容 |
|---|---|
| id | `t4-build-test-v3` |
| 種類 | (4) ビルド・テストを含む複数の手順 |
| 旧課題との違い | v2 は agents だけ。v3 は 3 crate（agents・apply-patch・event-bus）に同じ型の作業を 1 つずつ。agents は「権限の仕様変更で既存テストが落ちる → 直す」、apply-patch は「新しい API を足して公開する」、event-bus は「関数を既存のメソッドに置き換えて、既存のテストと import を直す」（clippy の unused import が出る）。fmt / clippy / test を 3 crate で通し、基準値・最終値・ロール・文字列を報告する |
| 準備 | なし（固定コミットのまま）。事前に `cargo fetch` などで依存を取得済みにしておく（3 crate とも libclang 不要） |
| 検証 | `verify/t4-build-test-v3.sh <clone>`（fmt / clippy / test / 隠しテストを 3 crate で実行） |
| 想定ターン数 | 20〜35（ビルド・テストは通常の shell ジョブで流すのが望ましい） |

## プロンプト

```prompt
次の 3 つの作業を行ってください。変更してよいのは `crates/agents`、`crates/apply-patch`、`crates/event-bus` の `src/` と `tests/` だけです（`answers/` への書き込みは除きます。手順 6 で `answers/t4.json` を書きます）。ワークスペース全体のビルドは不要です。依存の取得ができない環境では cargo に `--offline` を付けてください。event-bus のテストと clippy は `--features otel-exporter` 付きで流します（`cargo test -p event-bus --features otel-exporter`）。

1. 変更の前に、3 つの crate それぞれで `cargo test` を実行し、通ったテスト数の合計（すべてのテストバイナリと doc-test の `N passed` の合計）を控えます。

2. agents: Explorer ロールに `git_diff` と `skill_load` を許可します（`Role::capabilities()`）。`crates/agents/src/role.rs` の doc コメントのロール一覧の表も合わせて直します。期待値が合わなくなる既存テストは新しい仕様に合わせて直します（テストを削除したり `#[ignore]` を付けたりしてはいけません）。続けて `impl Role` に次の 2 つを足し、どちらにも doc コメントを付けます。
   - `pub const ALL: [Role; 9]` — 全 variant を enum の宣言順に並べる
   - `pub fn roles_allowing(tool: &str) -> Vec<Role>` — `Role::capabilities().allowed_tools` に `tool` を含むロールを宣言順に返す
   新規ファイル `crates/agents/tests/role_all.rs` に 3 件以上のテストを書きます（`ALL` の長さと `Role::from_name(role.name())` の往復、`roles_allowing("skill_load")` が宣言順で期待どおり、`roles_allowing("shell")` が Worker だけで存在しないツール名では空）。

3. apply-patch: `crates/apply-patch/src/apply.rs` に次を足し、`lib.rs` から `ChangeCounts` を公開します（すべて doc コメント付き）。
   - `impl ChangeKind { pub const fn label(&self) -> &'static str }` — `"added"` / `"updated"` / `"moved"` / `"deleted"`
   - `#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)] pub struct ChangeCounts { pub added: usize, pub updated: usize, pub moved: usize, pub deleted: usize }` と `pub const fn total(&self) -> usize`（4 つの合計）
   - `impl Applied { pub fn counts(&self) -> ChangeCounts }` — `changes` を種類ごとに数える
   新規ファイル `crates/apply-patch/tests/change_counts.rs` に 3 件以上のテストを書きます。1 件は、実際のパッチ（追加・更新・移動・削除を 1 つずつ含む）を一時ディレクトリに `apply_patch` で適用して、`counts()` と `total()` を確かめるものにしてください。

4. event-bus: `crates/event-bus/src/event.rs` の `ProviderFailureKind` に `pub const fn error_type(&self) -> &'static str` を足します（値は `rate_limited` `http` `timeout` `invalid_response` `transport` `server` `quota` `auth` `other`。`Http` は status を捨てる）。`crates/event-bus/src/otel/mod.rs` にある同じ対応の関数 `map_failure` は削除し、呼び出しと、それを使っている既存のテスト（テストの数は減らさない）を `error_type` に置き換えます。新規ファイル `crates/event-bus/tests/failure_error_type.rs` に 2 件以上のテスト（全 9 variant の値と、`Http` が status によらず同じ値）を書きます。

5. 3 つの crate で `cargo fmt -p agents -p apply-patch -p event-bus -- --check`、`cargo clippy -p agents -p apply-patch -p event-bus --all-targets --features event-bus/otel-exporter -- -D warnings`、各 crate の `cargo test` がすべて通る状態にします。

6. 結果を `answers/t4.json` に書きます（`answers/` は作成済みです。`baseline_passed` は手順 1 の控え、`final_passed` は手順 5 の最後の `cargo test` の合計）。形式は
   {"baseline_passed": {"agents": 数, "apply-patch": 数, "event-bus": 数},
    "final_passed": {"agents": 数, "apply-patch": 数, "event-bus": 数},
    "all_len": 9,
    "skill_load_roles": [`skill_load` を許可するロール名を宣言順に],
    "git_diff_roles": [`git_diff` を許可するロール名を宣言順に],
    "error_types": [`ProviderFailureKind` の 9 variant の `error_type()` を宣言順に]}
   です。ロール名と文字列は、実装した `roles_allowing` と `error_type` の結果と一致させてください。

確認の質問はしないでください。曖昧な点は妥当な判断で進め、判断したことと置いた仮定を `answers/notes.md` に書いてください。
```

## 想定解

- 手順 1 の合計は agents 28（`additional_roles` 6 + `capability_boundary` 22）、apply-patch 94、event-bus 175。
- agents: Explorer を `["read", "grep", "git_diff", "skill_load"]` にすると、`explorer_allows_exactly_adr_0002_tools`（`tests/support/capability_tools.rs` の `EXPLORER_TOOLS`）と `explorer_denies_skill_load` が落ちる。前者は定数に 2 ツールを足し、後者は「許可する」テストに書き換える。
- apply-patch: `ChangeKind::label`、`ChangeCounts`（`Default` 付き）と `total()`、`Applied::counts()`、`lib.rs` の `pub use` に `ChangeCounts`。
- event-bus: `ProviderFailureKind::error_type()`、`map_failure` の削除、呼び出し 1 か所と既存のテスト 1 件の置き換え。**`map_failure` を消すと `otel/mod.rs` の `use crate::event::{…, ProviderFailureKind, …}` がテスト以外で使われなくなり、`-D warnings` の clippy が unused import で落ちる**（テストモジュールに import を移す）。
- 最終は agents 28 + 4 = 32、apply-patch 94 + 3 = 97、event-bus 175 + 2 = 177（参照解）。`skill_load_roles` は Orchestrator, Explorer, Worker, Planner、`git_diff_roles` は Orchestrator, Explorer, Worker, Reviewer, Planner, Advisor。
- 参照解: `solutions/t4-build-test-v3.sh`。部分誤答は `solutions/t4-build-test-v3.wrong-*.sh`（壊れたテストを消す / 名前順に返す / `map_failure` を残す / `Moved` のラベルが違う）。

落とし穴:
- agents の変更後、テストの失敗は 1 回目の `cargo test` で初めて分かる。`cargo test` はバイナリごとに失敗で止まるので、`--no-fail-fast` を付けないと残りが見えない。
- 壊れたテストを消せば通るが、テスト数の下限（既存 28 件、otel/mod.rs の既存テスト数）で検出される。doc の表の更新を忘れやすい。
- `Moved` のラベルは `"moved"`。`Applied::counts` のテストは、`Moved` が `updated` に数えられていないかを見る（`Updated` + `Moved` の両方の分が合うこと）。
- event-bus は feature 付きでテスト・clippy を流さないと、`span_exporter.rs` 側の取りこぼしに気づけない。
- clippy の `-D warnings` は `--all-targets` なので新規テストも対象。
- 合計の数え方（バイナリごとの `N passed` を足す）と、手順 1 と最終の取り違え。3 crate で別々に数える。

## 想定ターン数の根拠

3 つの作業それぞれが「読む（既存の型・テスト）→ 書く → 動かす → 直す」を持ち、(1) 基準値の取得 3 回、(2) agents は 1 回目のテストで 2 件落ちて直す（4〜6 リクエスト）、(3) apply-patch は API を足してテストを書く（パッチの書き方を既存テストから調べる、4〜6）、(4) event-bus は置き換えと clippy の unused import の手直し（4〜6）、(5) fmt / clippy / test の通し（2〜4）、(6) 報告（1〜2）。全体で 20〜35 リクエスト（v2 の実測 7〜12 の約 3 倍）と見る。

## 採点基準

- 機械採点（合否）: ①変更が 3 crate の `src/`・`tests/` に限られ、3 つの新規ファイルのテストが 3・3・2 件以上 ②agents の既存テストが 28 件以上、event-bus の `otel/mod.rs` の既存テストが減っておらず、`#[ignore]` が増えていない、`fn map_failure(` が無い ③`role.rs` の doc 表の Explorer の行に `git_diff` と `skill_load` ④`cargo fmt`（3 crate）⑤`cargo clippy -D warnings`（3 crate、event-bus は feature 付き）⑥各 crate の `cargo test` が成功し、passed 合計が元 + 新規テスト数以上 ⑦`answers/t4.json` の 7 項目が正解 ⑧隠しテスト（agents: `Role::ALL` の順序、Explorer の権限、`roles_allowing`。apply-patch: 4 種のラベル、`ChangeCounts`、実パッチの `counts()`。event-bus: `error_type` の全値と const 評価）が成功。
- 追加観察: ビルド・テストの実行方法（通常の shell ジョブか、`run_js` の中か）、1 回の shell に連結した手順の数（build 連結数）、手順 1（変更前の実行）を省略していないか、失敗したテスト・clippy の読み方と修正までのターン数。
- LLM 採点の観点: 書き換えたテストが空虚でないか、doc コメントの質、`answers/notes.md` に仮定が書かれているか。

## 検証コマンド

```sh
evals/run-js/verify/t4-build-test-v3.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由（と効かない部分）

3 crate の既存コード・テストの調査（`role.rs`、`tests/`、`apply.rs`、`event.rs`、`otel/mod.rs`）と、複数ファイルの編集は `run_js` でまとめられる。一方、中心の cargo の実行は ADR 0039 が「ビルドや長いテストは `run_js` の中で実行せず、通常の shell ジョブとして実行する」としているので、「向く部分と向かない部分が混ざる多段の作業で使い分けられるか」を見る。`run_js` の中で `cargo` を呼ぶ・ビルドと確認を 1 つのワンライナーに連結するといった誤用を観察する。
