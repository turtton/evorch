# v14-e-file-tool-boundary Implementation Packet

## Goal

ファイルツールが昇格なしで触れる範囲を shell の sandbox と同じにし、境界の外へは shell と同じ gate で 1 回ごとに昇格させる。

## Why

ファイルツールは本体プロセスで動き、パスの境界を持たない。shell だけを sandbox に閉じ込めても、`read` や `write` に絶対パスを渡せば境界を素通りできる。ADR 0031 で isolated workspace をやめると、この穴が守るべき境界の穴になる。

## Scope

- **境界の一覧:** sandbox crate（または runtime）に、読み書き可の領域と読み取り専用の領域を持つ型を置き、1 つの関数で組み立てる。中身は、読み書き可がプロジェクトルート（shared run の workspace root）とその thread の一時領域（v14-d）、読み取り専用が `DEFAULT_RO_BINDS`、`output_root`、Cargo / rustup のキャッシュのホスト側のパス。`BwrapConfig`（`ro_binds` / `rw_binds` / キャッシュのマウント）とファイルツールの判定の両方をこの一覧から作る。許可ディレクトリ（v14-f）を追加できる口を用意する。
- **判定の位置:** `ToolExecutor::execute_inner` の `scoped_args` の後で判定する。判定には run ごとの executor に設定した境界の一覧を使う（`configured_executor` で一時領域とともに設定する）。benchmark の `validate_workspace_args` は残し、benchmark ではそちらを優先する。
- **パスの判定:** `workspace_boundary.rs` の判定（絶対パス必須、`..` の扱い、存在する最も深い祖先の canonicalize、通常ファイル / ディレクトリ以外の拒否）を、複数の領域と読み取り専用の区別に広げる。`read` は path / file / file_path / filename / target、`grep` / `write` / `edit` は path を対象にする。`grep` は rg に `--follow` を付けないことを前提にし、テストで固定する。
- **昇格の引数:** 4 つのツールのスキーマに `sandbox_access`（`"isolated"` / `"unsandboxed"`、既定 `"isolated"`）と `justification` を追加する。shell の説明文と同じく、`"isolated"` は sandbox の境界の中で実行するという意味で、isolated workspace とは関係がないことを書く。`"network"` は持たない。
- **gate:** `ShellEscalationGate` を、shell のコマンドとファイルアクセスの両方を表す要求（ツール名、対象パス、理由、`write` / `edit` の差分の要約）を受けるツール共通の trait に広げる。`SandboxEscalationGate` は auto / user / off と `escalate_to_user_on_deny` をそのまま適用する。auto 用に `QuickModelReviewer` へファイルアクセス専用の審査（`review_web_fetch` と同じ形）を加える。user の承認要求は `ApprovalGate.request_with_input` に `kind: "file_access"` とツール名・パス・理由を渡す。`configure_shell_escalation` で、shell と同じ gate をファイルツールにも設定する。
- **role の権限:** 昇格で外すのはパスの境界だけにする。ツールの可視性（`visible_tool_specs` と policy）は変えず、読み取り専用の領域への書き込みを昇格で許すのは、そのツールを持つ role の場合だけにする。
- **拒否のメッセージ:** 境界の外であること、`sandbox_access: "unsandboxed"` と `justification` で昇格を求められること、繰り返し使う場所なら許可ディレクトリとして登録してもらえることを伝える。拒否した呼び出しの結果にファイルの中身を含めない。
- **benchmark:** `filter_benchmark_tools` で 4 つのツールのスキーマから `sandbox_access` / `justification` を取り除く。
- **GUI:** 承認表示が `kind: "file_access"` の要求でツール名・パス・理由を表示できるようにする。
- **Docs:** `docs/harness-reliability.md`。

### 採用・却下案と理由

- 読み取りだけ境界の外を許す案は採らない（ADR 0033 で shell と同じ gate を使う方針に合意済み）。依存ライブラリのソースや `/nix/store` は境界の中なので、通常の作業は止まらない。
- 承認を同じパスやディレクトリに対して再利用する案は採らない。1 回の呼び出しに限り、繰り返し使う場所は許可ディレクトリとして登録してもらう。
- ファイルツールごとに別の gate を作る案は採らない。審査の設定と GUI の承認の流れを shell と共有するため、1 つの gate に広げる。

## Out of scope

許可ディレクトリの受け渡しとマウント（v14-f）。承認の再利用。`sandbox_access` の値の改名。shell の挙動の変更。

## Verification

### Unit

- 境界の一覧の組み立てと、それから作る `BwrapConfig` のマウントが一致すること。
- 読み取り専用 / 読み書き可の判定。`..`、シンボリックリンク（境界の中のリンクが外を指す場合、外のリンクが中を指す場合）、存在しないファイルの書き込み先、特殊ファイル。
- `"unsandboxed"` で justification が空の場合の拒否。gate の auto / user / off と `escalate_to_user_on_deny` の分岐。
- benchmark のスキーマの絞り込み。

### Integration → build

- Worker が境界の外のファイルを `read` / `write` すると拒否され、`"unsandboxed"` で承認された場合だけ成功すること。拒否の結果にファイルの中身が出ないこと。user の承認は承認・拒否のイベントを待って判定し、sleep で待たない。
- 一時領域、プロジェクトルート、Cargo の registry のソースは昇格なしで読めること。読み取り専用の領域への `write` は拒否されること。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- 境界の一覧の型を sandbox crate と runtime のどちらに置くかは、v14-c で `IsolatedMounts` がどう変わるかに合わせて実装時に決める。
- ツールのスキーマが変わるので、システムプロンプトやツール定義を固定しているテスト（`crates/runtime/tests/prompt_end_to_end.rs` など）の期待値を更新する範囲は実装時に確かめる。
