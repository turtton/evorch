# ADR 0032: プロジェクトの許可ディレクトリを runtime とエージェントに届ける

## Status

Accepted（2026-10-13）。利用者との設計相談で方針に合意した。[ADR 0003](0003-cache-first-context-engine.md) の Stable Prefix を保ったまま、会話の途中で変わる許可ディレクトリをエージェントへ伝える方法を定める。

## Context

プロジェクトには、リポジトリルートの外にあるディレクトリを `allowed_directories` として登録できる（`crates/workspace-ui/src/project.rs` の `ProjectRecord`、各要素は path と trust を持つ）。GUI のプロジェクトダイアログで追加・表示・trust でき、`resolve_membership` でパスの所属も判定できる。

ただし、ここで止まっていて runtime には届いていない（2026-10-13 時点）。

- GUI から runtime に渡るのは、プロジェクトルート（`set_project_root`）とプロジェクト単位の trust（`set_project_trust`）だけである。
- システムプロンプトには許可ディレクトリが載らないので、エージェントは存在を知らない。
- shell の bwrap サンドボックスは、既定の読み取り専用パス、workspace、git common dir、出力ディレクトリしかマウントしない。許可ディレクトリは shell から見えない。
- `run_membership`（`crates/gui/src/model/project_bridge.rs`）はテストからしか呼ばれていない。
- プロジェクトルール（AGENTS.md など）の探索は、プロジェクトルートの外に出ると `EscapedRoot` になる（`crates/runtime/src/rules/discovery.rs`）。

許可ディレクトリはスレッドの途中でも追加・削除・trust 変更される。システムプロンプトにそのまま載せて変更のたびに書き換えると、送信済みの prefix が変わり、スレッドの prompt cache がそこで全部失われる。

## Decision

### 対象は trust 済みの許可ディレクトリだけ

- `TrustState::Approved` の許可ディレクトリだけを runtime に渡す。`Unapproved` のものは、マウントもプロンプトへの記載も指示ファイルの読み込みも行わない。GUI で Trust を押した時点で有効になる。
- 許可ディレクトリの trust は「アクセスを許すこと」と「その配下の指示ファイルを読み込むこと」を兼ねる。未 trust のまま中身だけ見せる状態は作らない。
- GUI は、プロジェクトの trust 済み許可ディレクトリの一覧を runtime に渡す。`set_project_root` / `set_project_trust` と同じく、プロジェクトのルートをキーにして宣言する。

### アクセス権は role に従う

- アクセス権は role の権限に従う。書き込み権限を持つ role（Worker など）は読み書き、それ以外は読み取り専用とする。isolated workspace は [ADR 0031](0031-remove-isolated-workspaces.md) で廃止するので、run の種類による特例は設けない。
- shell では bwrap のマウントに含める。読み取り専用なら `ro_bind`、読み書きなら `bind`。今のマウントの一覧は run の sandbox を作った時点で固定される（`factory.build`）。許可ディレクトリの分はこれを改め、コマンドを実行するたびにその時点の一覧から組み立てる。これで、変更は次のコマンドから効く。ただし、実行中のバックグラウンドの shell ジョブは、開始時のマウントのまま終了まで動く。
- 許可ディレクトリの中で worktree を使って作業したい場合は、プロジェクトルートと同じく、エージェントが skill の手順でその許可ディレクトリの中に worktree を作る。読み書き可の許可ディレクトリであれば、sandbox の中から作れる。

### システムプロンプトはスレッド開始時の一覧で固定する

- run の開始時に、その時点の一覧（パスとアクセス権）を、workspace note の隣にシステムメッセージの一節として載せる。これが run の Stable Prefix になる。
- 以後、その run のシステムメッセージは許可ディレクトリの変更では書き換えない。workspace note と違い、復元時にも書き換えない。
- 委譲で新しく始まる子 run は、開始時点の一覧を自分のシステムメッセージに持つ。新しい run は新しい prefix なので、cache への影響はない。

### 途中の変更は次のターンで末尾に追記する

- 一覧が変わったら、その run の次のモデル呼び出しの前に、前回伝えた一覧との差分を履歴の末尾へ追記する（例: `[workspace update] 追加: /path (read-only)` / `削除: /path`）。作業中に見つかったルールの追記（`crates/runtime/src/agent_loop/tool_calls.rs` の `rules::after_successful_tools` → `push_user`）と同じ経路を使う。
- 送信済みの prefix は変わらないので cache は保たれる。増えるのは追記した suffix の分だけである。
- 権限の強制は、通知の追記を待たずに変更時点から行う。削除や trust の取り消しは、次のコマンドやツール呼び出しから効く。通知は、エージェントが古い一覧を前提に動かないようにするためのものである。
- run ごとに「最後に伝えた一覧」を持ち、復元用の run スナップショット（[ADR 0027](0027-restore-contract.md)）に保存する。復元時はシステムメッセージを書き換えず、保存した一覧と現在の一覧の差分を同じ方法で追記する。
- compaction では、AGENTS.md の cache 契約どおりシステムメッセージとツールを変えない。compaction 後の履歴に現在の一覧を書き直し、それを新しい「最後に伝えた一覧」とする。

### 指示ファイルは、初めて触ったときに読み込む

- 起動時に読み込む指示ファイル（AGENTS.md などのルールと repository skills）は、これまでどおりプロジェクトルートのものだけとする。許可ディレクトリのものは、システムプロンプトに入れない。
- エージェントのツールが trust 済み許可ディレクトリの中のパスを初めて対象にしたとき（`read` / `grep` / `write` / `edit` のパス、shell の `cwd`）に、その配下の指示ファイルを末尾へ追記する。
  - ルール: 許可ディレクトリをもう 1 つのルートとしてルール探索に加え、そのルートから対象パスまでの階層で見つかったルールを追記する。同じルールは run の中で 1 回だけ注入し、`max_injection_bytes` の予算に従う。今のプロジェクトルールの注入には重複を防ぐ仕組みがなく、対象パスに当たるルールがツールの往復ごとに入り直す。許可ディレクトリの分は、注入済みのルールを run ごとに記録して重複を防ぐ。プロジェクトルートの分を同じように直すかどうかは、この ADR では決めない。
  - skills: その許可ディレクトリの repository skills の一覧を追記し、その run の中では `skill_load` で読み込めるようにする（delegate の `load_skills` で子 run に渡す場合も含む）。run 単位で skill registry を広げるだけで、共有の skill catalog とシステムプロンプトの skills 節は変えない。
- shell のコマンド引数に書かれたパスは検出しない。`cwd` だけを手がかりにする。

## Consequences

- 許可ディレクトリを足しても、進行中のスレッドの cache は失われない。追記した分の suffix が増えるだけである。
- 新しいスレッドは開始時点の一覧をシステムプロンプトに持つ。スレッドの途中から使い始めたディレクトリは、システムプロンプトではなく履歴の中にしか現れない。
- 許可ディレクトリに書き込んだ変更は、プロジェクトルートの git diff を表示する diff パネルには現れない。
- 許可ディレクトリの指示ファイルは、エージェントが実際にそこを触るまで読まれない。触る前の判断は、プロジェクトルートの指示だけに基づく。
- 実装は 3 段に分ける。①runtime への一覧の受け渡しと bwrap のマウント、②システムメッセージの一節と途中変更の追記（スナップショットへの保存、復元と compaction）、③初めて触ったときのルールと skills の読み込み。②は cache 契約に関わるので、`scripts/check-cache-contracts.sh` の end-to-end シナリオを追加する（途中で追加しても以前の wire prefix が保たれること、追記後のターンで新しい prefix が保たれること）。

## Open questions

- ~~直接のファイルツール（`read` / `grep` / `write` / `edit`）にこの ADR のアクセス権を効かせるかどうか~~ → [ADR 0033](0033-file-tool-path-boundary.md) で決定。ファイルツールの境界は shell の sandbox と同じ一覧から組み立てるので、許可ディレクトリのアクセス権もそのまま効く。

## Related

- [ADR 0002](0002-role-capability-boundaries.md)、[ADR 0003](0003-cache-first-context-engine.md)、[ADR 0021](0021-bwrap-linux-sandbox.md)、[ADR 0027](0027-restore-contract.md)、[ADR 0031](0031-remove-isolated-workspaces.md)、[ADR 0033](0033-file-tool-path-boundary.md)
- [features/gui-workbench](../features/gui-workbench/overview.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
- [features/context-engine](../features/context-engine/overview.md)
