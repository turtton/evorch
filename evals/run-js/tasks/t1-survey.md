# t1-survey — 調査と要約

| 項目 | 内容 |
|---|---|
| id | `t1-survey` |
| 種類 | (1) 複数ファイルにまたがる調査と要約 |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t1-survey.sh <clone>` |
| 想定ターン数 | 5〜10（`run_js` なしなら 12〜20） |

## プロンプト

```prompt
このリポジトリは Rust のワークスペースです。`crates/tools/src/tools/` 以下で `Tool` トレイトを実装している型を調べて、一覧を作ってください。ブラウザ関連（`browser.rs` と `browser/`）は対象外です。

各ツールについて、次の項目を調べます。
- name: `Tool::name()` が返すツール名（定数経由で返す場合は、その定数の値）
- execution_mode: `Tool::execution_mode()` の値（`"Shared"` か `"Exclusive"`。メソッドを上書きしていなければトレイトの既定値）
- fs_read / fs_write / process_spawn / network: `Tool::permissions()` が返す `Permissions` の各フラグ（true / false）
- worker_allowed: Worker ロールの `allowed_tools`（`crates/agents` の `Role::Worker.capabilities()`）にそのツール名が含まれるか（true / false）

結果を JSON 配列として `answers/t1.json` に書いてください（`answers/` は作成済みです）。要素は次の形で、name の昇順に並べます。

{"name": "...", "execution_mode": "Shared", "fs_read": true, "fs_write": false, "process_spawn": false, "network": false, "worker_allowed": true}

ソースコードは変更しないでください。
```

## 想定解

9 ツール（name 昇順）:

| name | execution_mode | fs_read | fs_write | process_spawn | network | worker_allowed |
|---|---|---|---|---|---|---|
| apply_patch | Exclusive | ✓ | ✓ | | | ✓ |
| edit | Exclusive | ✓ | ✓ | | | ✓ |
| git_diff | Shared | ✓ | | ✓ | | ✓ |
| grep | Shared | ✓ | | | | ✓ |
| read | Shared | ✓ | | | | ✓ |
| shell | Exclusive | ✓ | ✓ | ✓ | | ✓ |
| web_fetch | Shared | | | | ✓ | |
| web_search | Shared | | | | ✓ | |
| write | Exclusive | ✓ | ✓ | | | ✓ |

機械可読な正解は `verify/expected/t1-survey.json`。

落とし穴:
- `apply_patch` の名前は `name()` が `apply_patch::tool_contract::NAME`（`crates/apply-patch/src/tool_contract.rs`）を返すため、別の crate を読まないと分からない。
- `git_diff` の `permissions()` は `Permissions::…()` ではなくフィールドの直書きで、`process_spawn` が true。
- 権限は `Permissions::read_only()` / `read_write()` / `process()` / `network()`（`crates/tools/src/tool.rs`）の定義を読んで展開する必要がある。
- `Tool` を実装する型は `browser.rs` や `runtime` などにもある。対象は `tools/` 直下（ブラウザ除く）の 9 つだけ。

## 採点基準

- 機械採点（合否）: 要素数 9、name の昇順、全 63 項目（9 ツール × 7）の計 65 項目が正解と一致。1 項目でも違えば不合格。検証は不一致の項目を列挙する。
- 追加観察: 一致項目の割合（部分点）、`apply_patch` の name を正しく解決したか、ソースを変更していないか。
- LLM 採点の観点: 推測で埋めずにソースを読んだか（`execution_mode` を既定値と決めつけていないか）。

## 検証コマンド

```sh
evals/run-js/verify/t1-survey.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

9 ファイル + `tool.rs` + `role.rs` + `tool_contract.rs` の読み取りと grep がすべて独立で、`Promise.all` で 1 回にまとめられる。`grep` で `fn name` / `execution_mode` / `permissions` の周辺を絞って抽出し、結果の JSON 組み立ても JS 内で完結する。`run_js` なしでは read / grep を 1 ターン 1 回ずつ呼びがちで、10 ターン以上になりやすい。
