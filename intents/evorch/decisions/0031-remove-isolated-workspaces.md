# ADR 0031: isolated workspace とワークスペース snapshot を廃止する

## Status

Accepted（2026-10-13）。利用者との設計相談で方針に合意した。[features/agent-runtime-kernel](../features/agent-runtime-kernel/overview.md) の `RunConfig.workspace_mode` と git worktree backend、[features/orchestration](../features/orchestration/overview.md) の「isolated workspace の正式運用」、[features/gui-workbench](../features/gui-workbench/overview.md) の Git snapshot undo/redo を置き換える。[ADR 0025](0025-agent-experience-evaluation.md) が残した論点「並列書き込み時の Shared/Isolated 既定ポリシー」は、この ADR で閉じる。

## Context

isolated workspace は「委譲した run ごとに runtime が git worktree を作り、その中で実行させる」仕組みである（`crates/runtime/src/workspace.rs`、`<root>/.evorch/worktrees/<run>`、ブランチ `evorch/task/<run>`）。2026-10-13 時点の状態は次のとおりで、効いている範囲が狭い。

- メインの thread（orchestrator）は常に shared で、プロジェクトの checkout の上で動く。GUI から isolated を選べるのは、context inspector のプレビューにあるチェックボックスだけである。
- isolated になるのは、モデルが delegate に `workspace_mode: "isolated"` を付けたときだけである（既定は shared）。runtime から指定しているのは、旧 PR delivery supervisor の Reviewer だけである。
- 隔離が漏れている。`read` / `write` / `edit` / `grep` は bwrap の外の本体プロセスで動くため、絶対パスを渡せば元の checkout に届く。isolated な run の子 run は、親の worktree ではなくプロジェクトルートを引き継ぐ（`crates/runtime/src/runtime/project.rs` の `bind_project`）。
- restore、エスカレーションの handoff、cancellation、merge mode、GUI の表示まで、runtime の約 40 ファイルと gui の約 10 ファイルに広がっている。

実際の運用では、worktree-pr skill を読んだ Worker や orchestrator が、自分でプロジェクトの中に worktree（例: `.worktrees/<name>`）を作ってその中で作業しており、問題なく回っている。bwrap はプロジェクトルートを読み書き可でマウントし、git common dir もその中にあるので、sandbox の中から `git worktree add` が通る。

これとは別に、書き込み系のツール呼び出し（`write` / `edit` / `shell`）は、毎回プロジェクトルート全体のロックを取って snapshot を撮る（`crates/runtime/src/agent_loop/snapshots.rs`、`crates/runtime/src/snapshot/`）。目的は `/undo` / `/redo` で run 単位に変更を戻すことで、並行する run の変更が混ざらないように 1 本ずつ通している。shell は書き込むかどうかを事前に判別できないので、`gh pr checks` のような読み取りだけのコマンドもロックを待つ。裏で動き続ける shell ジョブはロックを持ったままになるため、ロックの開放待ちが日常的に起きている。ロックの単位を worktree や cwd ごとに細かくしても、同じ worktree の中の shell 同士の待ちは残る。

## Decision

### isolated workspace mode を廃止する

- すべての run はプロジェクトルート（と [ADR 0032](0032-project-allowed-directories.md) の許可ディレクトリ）を共有する。
- 削除するもの: `WorkspaceMode` / `MergeMode`、delegate の `workspace_mode` / `workspace_branch` 引数、`WorktreeManager` と `OwnedWorktree`、isolated 用の bwrap マウント構成、エスカレーションでの worktree の引き渡し、restore 記述子の worktree 情報、cancellation での worktree 後始末、`<root>/.evorch/worktrees/` の membership（workspace-ui の `Membership::RuntimeWorktree`）、context inspector の Isolated チェックボックス、GUI の isolated 表示。
- AGENTS.md の互換方針（v0.x では内部の永続化形式の互換を保たない）に従い、移行層は作らない。
- **名前の注意**: shell の引数 `sandbox_access: "isolated"`（[ADR 0033](0033-file-tool-path-boundary.md) でファイルツールにも加える）は「sandbox の境界の中で実行する」という意味で、ここで廃止する isolated workspace とは関係がない。削除作業で `sandbox_access` の `"isolated"` を巻き込まないこと。

### worktree での作業はエージェントの運用に任せる

- runtime は worktree を作らず、使うことも強制しない。worktree を作るか、どこに作るか、いつ片付けるかは、エージェントが skill の手順に従って決める。
- worktree は sandbox が読み書き可でマウントしている場所に置く。プロジェクトルートの中（例: `.worktrees/`）か、読み書き可の許可ディレクトリの中である。外に置くと sandbox の中から書き込めない。
- 並列で書き込む Worker を別々の worktree に分けるのは、プロンプトと skill の責務とする。[ADR 0025](0025-agent-experience-evaluation.md) の「隔離なしの大並列は不採用」は、runtime ではなくこの運用で守る。

### ワークスペース snapshot、ロック、`/undo` / `/redo` を廃止する

- 書き込み系のツール呼び出しの前に snapshot を撮る処理と、それに伴うプロジェクトルート単位のロックを削除する。shell はロックを待たなくなる。
- `/undo` / `/redo` コマンド、`restore_snapshot`、`SnapshotService` / `SnapshotStore`、ロックの持ち主を伝えるイベント（`WorkspaceLockHolder` / `WorkspaceWait`）と GUI の待ち表示を削除する。
- 巻き戻しは git で行う（commit、ブランチ、worktree の破棄）。

### 旧 PR delivery supervisor を削除する

- goal から PR、Reviewer、merge 承認、closeout までを進める旧 supervisor（`crates/runtime/src/orchestration/`、isolated の Reviewer を起動する経路を含む）を削除する。汎用 goal は [ADR 0028](0028-thread-goals-and-completion-checks.md) の thread goals が担っている。
- ゲート、Reviewer の結果契約、goal のキャンセルなど、ほかから参照されている部分は、実装時に依存を洗って、残すか thread goals 側へ移すかを決める。[ADR 0026](0026-durable-execution-substrate.md) の durable execution の語彙のうち、storage や GUI が共有しているものも同様に扱う。

## Consequences

- shell のロック待ちがなくなる。並列の Worker が互いの `cargo test` や `gh pr checks` を待たなくなる。
- 同じ checkout に並列で書き込む run どうしの衝突を、runtime は防がない。防ぐのは worktree を分ける運用である。
- evorch の中から、未 commit の編集を 1 手ずつ戻すことはできなくなる。
- runtime と gui から、isolated、snapshot、旧 supervisor の分のコードとテストが減る。restore の契約（[ADR 0027](0027-restore-contract.md)）からも worktree の扱いが消える。
- 許可ディレクトリ（[ADR 0032](0032-project-allowed-directories.md)）は、isolated の特例なしで、role の権限どおりに扱える。ほかのリポジトリで作業する場合も、エージェントがその許可ディレクトリの中に worktree を作ればよい。
- 実装は 3 段に分ける。①ワークスペース snapshot、ロック、`/undo` / `/redo` の削除、②isolated workspace の削除、③旧 PR delivery supervisor の削除。①は単独で入れられ、効果がいちばん大きい。

## Open questions

- ~~直接のファイルツール（`read` / `write` / `edit` / `grep`）のパス制限~~ → [ADR 0033](0033-file-tool-path-boundary.md) で決定（shell の sandbox と同じ境界にそろえ、境界の外は shell と同じ gate で昇格する）。
- プロジェクトの中に置いた worktree とルール探索の関係。`.worktrees/x/` の中のパスを触ると、ルール探索はルートから `.worktrees/x/` までの各階層を拾うので、ブランチ側の AGENTS.md の写しが入れ子のルールとして注入される（`crates/runtime/src/rules/discovery.rs` の `chain_for_target`）。入れ子の git worktree をルール探索の起点として扱うかどうかを決める。
- worktree での作業手順を、組み込みの skill やプロンプトとして evorch に同梱するかどうか。

## Related

- [ADR 0021](0021-bwrap-linux-sandbox.md)、[ADR 0025](0025-agent-experience-evaluation.md)、[ADR 0026](0026-durable-execution-substrate.md)、[ADR 0027](0027-restore-contract.md)、[ADR 0028](0028-thread-goals-and-completion-checks.md)、[ADR 0032](0032-project-allowed-directories.md)、[ADR 0033](0033-file-tool-path-boundary.md)
- [features/agent-runtime-kernel](../features/agent-runtime-kernel/overview.md)
- [features/orchestration](../features/orchestration/overview.md)
- [features/gui-workbench](../features/gui-workbench/overview.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
