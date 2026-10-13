# ADR 0032 ①: trust 済み許可ディレクトリの runtime への受け渡しと sandbox へのマウント

## Goal

プロジェクトに登録して trust した許可ディレクトリを、エージェントが実際に使えるようにする。GUI から runtime に一覧を渡し、shell の sandbox とファイルツールの境界に、role の権限どおりのアクセス権で加える。ADR 0032 の後続（プロンプトへの記載、指示ファイルの読み込み）の前提となる基盤である。

## Why This Slice Exists Now

許可ディレクトリは GUI で追加・表示・trust できるが、runtime には一切届いていない。shell の sandbox にマウントされず、エージェントからは見えない。ADR 0033 でファイルツールにも境界が入ると、許可ディレクトリを境界に加えない限り、昇格なしでは触れなくなる。

## Current Observed State

- `ProjectRecord.allowed_directories`（`crates/workspace-ui/src/project.rs`）はパスと trust を持つが、GUI から runtime に渡るのはプロジェクトルート（`set_project_root`）とプロジェクト単位の trust（`set_project_trust`）だけ。
- `run_membership`（`crates/gui/src/model/project_bridge.rs`）はテストからしか呼ばれていない。
- shared run の sandbox は run の開始時に `factory.build` で作られ、`BwrapConfig` のマウント一覧は sandbox の生成時に固定される。argv はコマンドごとに組み立てるが、一覧そのものは途中で変わらない。
- 書き込みツール（`write` / `edit` / `shell`）を持つ role は Worker だけ。

## Accepted Baseline You May Assume

- isolated workspace は v14-c で廃止済み。run の種類による特例は設けない。
- v14-e で、bwrap のマウントとファイルツールの判定が共有する境界の一覧が 1 か所に作られている。この unit はそこに許可ディレクトリを加える。
- GUI の永続化形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/workspace-ui/` `crates/gui/` `crates/runtime/` `crates/sandbox/` `crates/tools/` `docs/` `intents/`
- Target part: GUI から runtime への許可ディレクトリの宣言、runtime のプロジェクト単位の保持、境界一覧への追加と role ごとのアクセス権、走行中の run への反映。

## In Scope

- GUI が、プロジェクトのルートをキーにして trust 済みの許可ディレクトリの一覧を runtime に宣言する API（`set_project_trust` と同じ形）。Trust・追加・削除・trust の取り消しのたびに宣言し直す。
- runtime がプロジェクト単位で一覧を保持し、run はその run のプロジェクトの一覧を参照する。
- v14-e の共通の境界一覧に、書き込みツールを持つ role では読み書き可、それ以外では読み取り専用として加える。
- 走行中の run でも、変更が次のコマンド・次のツール呼び出しから効くようにする（sandbox がマウント一覧を生成時に固定しないようにする）。
- `docs/` への記載。

## Out Of Scope

システムメッセージへの記載と途中変更の追記（v14-g）。許可ディレクトリのルールと skills の読み込み（v14-h）。未 trust のディレクトリの中身を見せる仕組み。

## Acceptance Criteria

1. GUI がプロジェクトごとに trust 済みの許可ディレクトリだけを runtime に宣言し、変更のたびに更新する。
2. run はその run のプロジェクトの一覧だけを使う。
3. 書き込みツールを持つ role では読み書き可、それ以外では読み取り専用でマウントされる。
4. 許可ディレクトリは共通の境界一覧に入り、ファイルツールも同じ範囲・同じアクセス権で扱える。
5. 一覧の変更は、走行中の run の次のコマンド・次のツール呼び出しから効く。
6. 存在しないパスなどは run の開始を妨げない。
7. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 宣言から未 trust を除くこと、role ごとのアクセス権の決定、境界一覧への追加と削除。
2. Integration: 走行中の Worker の run で、許可ディレクトリを追加した後の shell コマンドから見え、削除した後は見えないこと。読み取り専用の role の run で書き込みが失敗すること。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0032-project-allowed-directories.md`
- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
