# v14-h-allowed-directories-lazy-instructions Implementation Packet

## Goal

trust 済みの許可ディレクトリを初めて触ったときに、そこのルールと repository skills をエージェントに届ける。システムプロンプトと共有の skill catalog は変えない。

## Why

別のリポジトリの決まり（AGENTS.md や skills）を知らないまま作業すると、そのリポジトリの手順から外れる。起動時に全部読み込むとプロンプトが膨らみ、途中で追加された許可ディレクトリでは cache が崩れる。

## Scope

- **検出:** `tool_calls.rs` で `rule_targets` を集める箇所を広げ、成功した `shell` の `cwd` も対象にする。各対象を v14-f の一覧と照らし、trust 済み許可ディレクトリの中かどうかを判定する。プロジェクトルートの中の対象は従来の経路に残す。
- **ルール:** 許可ディレクトリの中の対象は、そのディレクトリをルートとして `chain_for_target` を呼ぶ。run ごとに注入済みのルールファイル（canonical path）を記録し、注入済みのものは除く。予算は `injection_budget_bytes` に従う。プロジェクトの trust ではなく、許可ディレクトリの trust（Approved であること）で可否を判定する。
- **skills:** 許可ディレクトリを初めて触ったとき、`repo_skill_dirs(<許可ディレクトリ>)` から skills を見つけ、一覧（名前と説明）を追記する。run 単位の registry に加え、`skill_load` と `validate_load_skills` がそれを参照するようにする。共有の `SkillCatalogSource` は変えない。名前が既存の skill と衝突した場合は、既存のものを優先して追記で知らせる。
- **子 run:** delegate の `load_skills` で許可ディレクトリの skill を渡した場合、子 run の registry にも同じ拡張を引き継ぐ。
- **記録:** 読み込み済みの許可ディレクトリ・ルールファイル・skills を run スナップショットに保存し、復元後に再注入しない。
- **追記の境界:** v14-g と同じく、完全なツールラウンドの後・次のプロバイダ入力の直前に 1 つのメッセージとして追記する。
- **Docs:** `docs/`。

### 採用・却下案と理由

- 起動時にすべての許可ディレクトリの指示を読み込む案は却下する（ADR 0032）。プロンプトが膨らみ、途中の追加で cache が崩れる。
- プロジェクトルートのルール注入にも重複除去を入れる案は、この unit では採らない。既存の挙動とテストに影響するため、必要なら別の unit にする。

## Out of scope

プロジェクトルートのルール注入の変更、shell のコマンド引数からのパスの検出、共有の skill catalog とシステムメッセージの skills 節の変更。

## Verification

### Unit

- 対象のパスと shell の cwd から、trust 済み許可ディレクトリの中を判定すること。未 trust や外のパスは対象外になること。
- 許可ディレクトリをルートとするルール探索と、注入済みのルールの除外。
- run 単位の registry の拡張と、名前の衝突時の扱い。

### Integration → build

- 許可ディレクトリの中のファイルを 2 回 read しても、ルールの追記が 1 回だけであること。
- skills を `skill_load` で読み込めること。delegate の `load_skills` で子 run に渡し、子 run で読み込めること。
- 復元後に同じルールと skills を再注入しないこと。
- cache の contract テストで、読み込みの追記の前後で wire prefix が保たれること。待機はイベント駆動で行う（test-policy）。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- 許可ディレクトリの中にさらに git worktree がある場合の扱いは、ADR 0031 の Open questions（入れ子の worktree とルール探索）と合わせて決める。
