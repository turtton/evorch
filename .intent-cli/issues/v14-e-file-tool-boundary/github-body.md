# ADR 0033 ②: ファイルツールのパス境界と shell と同じ gate による昇格

## Goal

`read` / `grep` / `write` / `edit` が昇格なしで触れる範囲を、shell の sandbox から見える範囲（ホスト側のパス）にそろえる。境界の外は、shell と同じ `sandbox_access: "unsandboxed"` と `justification` を付けて、同じ審査の gate を 1 回ごとに通す。

## Why This Slice Exists Now

ファイルツールは本体プロセスで動き、パスを制限していない。絶対パスを渡せば `~/.ssh` を読むことも、プロジェクトの外へ書き込むこともできる。tools-sandbox の intent にある「Worker = outside denied」は shell でしか守られていない。ADR 0031 で isolated workspace をやめると、守るべき境界はプロジェクトルートと許可ディレクトリになり、ファイルツールの穴がそのまま境界の穴になる。

## Current Observed State

- `ToolExecutor::execute_inner` は相対パスを cwd で絶対パス化したあと、ツール単位の承認判定だけを行う。パスの境界は benchmark の opt-in（`validate_workspace_args`）にしかない。
- shell には `sandbox_access` / `justification` と `ShellEscalationGate` による 1 回ごとの昇格がある。gate は shell 専用で、ファイルツールには経路がない。
- shell の sandbox の可視範囲は、`DEFAULT_RO_BINDS`、`output_root`、run ごとの `IsolatedMounts`、Cargo / rustup のキャッシュ、v14-d の一時領域で、それぞれ別の場所で組み立てられている。

## Accepted Baseline You May Assume

- v14-d で、一時領域は thread 単位の固定パスになっている。
- 許可ディレクトリ（ADR 0032、v14-f）はまだ runtime に届いていない。この unit では境界の一覧に許可ディレクトリを加える口だけを用意する。
- ツールのスキーマの変更は evorch の更新でだけ起き、セッションの途中では起きない。古いスキーマで記録した benchmark のチェックポイントは再生できなくなってよい（AGENTS.md の Compatibility policy）。
- ADR 0031 の isolated workspace の削除（v14-c）と前後しうる。`IsolatedMounts` の形に依存しすぎない。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/sandbox/` `crates/tools/` `crates/runtime/` `crates/gui/` `docs/` `intents/`
- Target part: 境界の一覧、ファイルツールのパス判定、ファイルツールの昇格の引数、gate の共通化、審査役への入力、GUI の承認表示、benchmark のスキーマの絞り込み。

## In Scope

- 境界の一覧（読み書き可 / 読み取り専用）を 1 か所で組み立てる型と関数。bwrap のマウントとファイルツールの判定の両方がそれを使う。
- `read` / `grep` / `write` / `edit` の判定。`read` / `grep` は境界の中、`write` / `edit` は読み書き可の領域の中なら昇格なし。それ以外は拒否し、昇格の引数と許可ディレクトリの登録を案内する。
- 4 つのツールへの `sandbox_access`（`"isolated"` / `"unsandboxed"`）と `justification` の追加。
- `ShellEscalationGate` をツール共通の gate に広げる。`SandboxEscalationGate` が、ツール名・対象パス・理由（`write` / `edit` は長さを制限した差分）を受けて、auto / user / off と `escalate_to_user_on_deny` に従って判定する。auto 用に、ファイルアクセス専用の審査の指示を `QuickModelReviewer` に加える。
- GUI の承認表示が新しい種類の要求（例: `kind: "file_access"`）を表示できること。
- パスの正規化とシンボリックリンクの判定を、`workspace_boundary.rs` の判定を複数の領域と読み取り専用の区別に広げて使う。
- benchmark でファイルツールのスキーマから `sandbox_access` / `justification` を取り除き、従来の固定した境界を使う。
- `sandbox_access` の `"isolated"` が isolated workspace と関係ないことの明示。
- `docs/harness-reliability.md` への記載。

## Out Of Scope

許可ディレクトリの受け渡しとマウント（v14-f）。同じ場所への承認の再利用やキャッシュ。`sandbox_access` の値の改名。shell の挙動の変更。

## Acceptance Criteria

1. 境界の一覧を 1 か所で組み立て、bwrap とファイルツールの両方がそれを使う。許可ディレクトリを後から足せる。
2. `read` / `grep` は境界の中、`write` / `edit` は読み書き可の領域の中なら昇格なしで動く。それ以外は拒否され、拒否のメッセージが昇格と許可ディレクトリの登録を案内する。
3. `"unsandboxed"` は空でない `justification` を必須とし、shell と同じ gate を 1 回ごとに通る。auto / user / off と `escalate_to_user_on_deny` が shell と同じように効く。
4. 審査役にツール名、canonicalize した対象パス、理由が渡り、`write` / `edit` では長さを制限した差分も渡る。GUI の承認表示が壊れない。
5. 昇格で role の権限は外れない。
6. `..` やシンボリックリンクで境界の外を指すパスは境界の外として扱い、通常ファイルとディレクトリ以外は拒否する。`grep` はシンボリックリンクをたどって境界の外の内容を返さない。
7. benchmark では従来どおり固定した境界を使い、昇格の引数をスキーマから取り除く。
8. スキーマとシステムプロンプトのテストが追従し、`"isolated"` の意味が明示されている。
9. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 境界の一覧の組み立て、読み取り専用と読み書き可の判定、`..` とシンボリックリンクと特殊ファイルの扱い、`"unsandboxed"` の justification の必須、gate の auto / user / off の分岐、benchmark のスキーマの絞り込み。
2. Integration: Worker が境界の外のファイルを `read` すると拒否され、`"unsandboxed"` で承認されたときだけ読めること。拒否されたときにファイルの中身が結果に出ないこと。一時領域とプロジェクトの中は昇格なしで読み書きできること。user の承認要求は、承認・拒否のイベントを待って判定する（sleep で待たない）。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- `intents/evorch/decisions/0002-role-capability-boundaries.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
