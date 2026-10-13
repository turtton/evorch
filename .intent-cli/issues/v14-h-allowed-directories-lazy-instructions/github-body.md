# ADR 0032 ③: 許可ディレクトリのルールと skills を初めて触ったときに読み込む

## Goal

別のリポジトリなどの許可ディレクトリで作業するとき、そこの AGENTS.md や skills がエージェントに届くようにする。読み込みはエージェントがそのディレクトリを初めて触ったときに行い、システムプロンプトと cache は変えない。

## Why This Slice Exists Now

v14-f / v14-g で許可ディレクトリを使えるようになり、エージェントもその存在を知る。しかし、そのディレクトリの指示ファイルは読まれないので、別のリポジトリの決まりを知らないまま作業することになる。起動時にすべての許可ディレクトリの指示を読み込むと、プロンプトが膨らみ、途中で追加されたときに cache が崩れる。

## Current Observed State

- ルールの途中注入は、成功した `read` / `write` / `edit` / `grep` の path を集め、ラウンドの後に `rules::after_successful_tools` の結果を追記する。shell の cwd は対象外。
- ルールの探索（`chain_for_target`）はプロジェクトルートの外に出ると `EscapedRoot` になる。
- 途中注入には run の中での重複除去がなく、対象に当たるルールはラウンドごとに注入され直す。
- skills の registry は run ごとに `state.skills()` で引き、`skill_load` と delegate の `load_skills` がこれを参照する。

## Accepted Baseline You May Assume

- v14-f で runtime が trust 済み許可ディレクトリの一覧を持つ。
- v14-g で、末尾への追記の境界と、run スナップショットへの状態の保存がある。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/runtime/` `docs/` `intents/`
- Target part: 初めて触ったことの検出、許可ディレクトリを追加のルートとするルール探索と重複除去、run 単位の skill registry の拡張と子 run への引き継ぎ、読み込み済みの記録。

## In Scope

- read / grep / write / edit のパスと shell の cwd から、trust 済み許可ディレクトリの中を初めて指したことを検出する。
- 許可ディレクトリをルートとしてルールを探索し、run の中で 1 回だけ注入する。予算に従う。
- その許可ディレクトリの repository skills の一覧を追記し、run 単位の registry に加える。delegate の `load_skills` で渡した子 run にも同じ拡張を引き継ぐ。
- 読み込み済みのルールと skills を run スナップショットに残し、復元後に再注入しない。
- `docs/` への記載。

## Out Of Scope

プロジェクトルートのルールの途中注入に重複除去を入れること（既存の挙動は変えない）。shell のコマンド引数からのパスの検出。共有の skill catalog とシステムメッセージの skills 節の変更。

## Acceptance Criteria

1. 許可ディレクトリの中を初めて触ったラウンドの後に、そこのルールが追記される。
2. 同じルールファイルは run の中で 1 回だけ注入され、予算に従う。
3. その許可ディレクトリの skills の一覧が追記され、`skill_load` と、delegate の `load_skills` で渡した子 run から読み込める。
4. システムメッセージの skills 節と共有の catalog は変わらない。
5. 未 trust のディレクトリや許可ディレクトリの外では読み込まない。
6. 読み込み済みのものが run スナップショットに残り、復元後に再注入しない。
7. 読み込みの追記の前後で wire prefix が保たれる。
8. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 初めて触ったことの検出、許可ディレクトリをルートとするルール探索、重複除去、run 単位の registry の拡張。
2. Integration: 許可ディレクトリの中のファイルを 2 回 read しても、ルールの追記が 1 回だけであること。skills を `skill_load` で読み込み、子 run に `load_skills` で渡せること。cache の contract テストで prefix が保たれること。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0032-project-allowed-directories.md`
- `intents/evorch/features/context-engine/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
