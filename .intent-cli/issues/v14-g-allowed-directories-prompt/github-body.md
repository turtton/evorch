# ADR 0032 ②: 許可ディレクトリの一覧をシステムメッセージで固定し、途中の変更を末尾に追記する

## Goal

エージェントが、使える許可ディレクトリとそのアクセス権を知ったうえで作業できるようにする。スレッドの途中で一覧が変わっても、送信済みの prefix を変えずに伝え、prompt cache を失わない。

## Why This Slice Exists Now

v14-f で許可ディレクトリは使えるようになるが、エージェントはその存在を知らない。システムプロンプトに載せて変更のたびに書き換えると、スレッドの cache がそこで全部失われる。

## Current Observed State

- 初期のシステムメッセージは `compose_system_sections`（`crates/runtime/src/base_context.rs`）が組み立て、preview も同じ sections を使う。
- workspace note は `update_workspace_system_message` が、開始時と復元時に既存の System メッセージの中で書き換える。
- 途中の状態を末尾へ追記する前例として、`append_todo_context`（`crates/runtime/src/thread_todos.rs`）がある。完全なツールラウンドの後、次のプロバイダ入力の直前にだけ追記し、compaction の後にフラグを立てて最新の状態を書き直す。

## Accepted Baseline You May Assume

- v14-f で、runtime はプロジェクト単位の trust 済み許可ディレクトリの一覧と、run ごとのアクセス権を持っている。
- run スナップショットの保存形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。
- compaction とモデル切り替えは、AGENTS.md の cache 契約上の明示的な境界である。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/runtime/` `crates/gui/` `docs/` `intents/`
- Target part: システムメッセージの新しい節、途中変更の差分の追記、最後に伝えた一覧の保存と復元、compaction 後の書き直し、cache の contract テスト。

## In Scope

- `ContextSectionKind` に許可ディレクトリの節を加え、`compose_system_sections` で run の開始時の一覧を出す（preview も同じ）。空なら出さない。
- run ごとに「最後に伝えた一覧」を持ち、変更があれば、完全なツールラウンドの後・次のプロバイダ入力の直前に差分を 1 つのメッセージとして追記する。
- 最後に伝えた一覧を `RunRestoreDescriptor` に保存し、復元時は差分だけを追記する。システムメッセージは書き換えない。
- compaction（モデル切り替えの境界を含む）の後、現在の一覧を書き直す。
- cache の contract テストの追加。`docs/` への記載。

## Out Of Scope

許可ディレクトリのルールと skills の読み込み（v14-h）。一覧の受け渡しとマウント（v14-f）。GUI での一覧の表示の変更。

## Acceptance Criteria

1. run の開始時の一覧がシステムメッセージの一節として載り、preview にも出る。空なら出さない。
2. 子 run は開始時点の一覧を自分のシステムメッセージに持つ。
3. 一覧の変更後、次のプロバイダ入力の直前に差分が 1 つだけ追記される。変更がなければ追記しない。
4. システムメッセージは、一覧の変更でも復元でも書き換わらない。
5. 最後に伝えた一覧が run スナップショットに保存され、復元後は差分だけが追記される。
6. compaction の後は現在の一覧が書き直され、システムメッセージとツールは変わらない。
7. cache の contract テストに、途中の追加・追記後のターン・復元・compaction のシナリオが加わる。
8. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 差分の計算（追加・削除・アクセス権の変更）、節の描画、復元時の差分。
2. Integration: mock provider の wire input で、途中で一覧を変えても以前の prefix がそのまま残り、追記の後のターンでその prefix が再利用されること。復元と compaction の後も同様。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0032-project-allowed-directories.md`
- `intents/evorch/decisions/0003-cache-first-context-engine.md`
- `intents/evorch/features/context-engine/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
