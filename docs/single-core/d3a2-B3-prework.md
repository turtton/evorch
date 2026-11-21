# D-3a2 B3 着手前作業：契約・対象表（計画 §3）

作成：worker。入力は `d3a2-B3-plan.md`（commit `c1385bdd`）§3、計画レビュー
`d3a2-B3-plan-review.md`（S1、S2 を反映）。

## 0. この文書の状態

- **コードの読解だけで作った。build も試験も実行していない。** 表の「製品 ELF から届くか」は
  読解による判断で、実行して確かめた事実ではない。実行で確かめたものには「実行済」と書く
  （この文書にはない）。
- 計画 §3 の 4 項目のうち、この文書が扱うのは **1（baseline）と 2（spawn 台帳）** だけ。
  **3（expiry 入口の実現性調査）と 4（測定条件の固定）は、この作業では行っていない。**
  親の判断 A、D（`d3a2-B3-plan.md` 末尾「親の判断」）に従い、別の作業として行う。
- 設計 v3 の §6.1、§8.2 は `d3a2-B-design-v3-excerpt.md` に復元した（復元できなかった部分もそこに書いた）。

## 1. baseline の固定（計画 §3-1）

| 項目 | 値 | 備考 |
|---|---|---|
| 作業時点の main | `1aa7d8993f720fb68784c9e583ddd00774a5eb80` | `ci: unify the dependency cache under a single writer and split CEF E2E in two (#752)` |
| 計画の commit | `c1385bdd53117e103cc1a429058981bbdbca14fe` | `docs: add D-3a2 B3 split plan` |
| #750（B2b-iii）の merge | `3769675db169477b36623fe06db5b4f1f712152f` | main に含まれる。この表の「現在の実装」はこれを含む main を読んだもの |
| #751 の merge | `a8030f4dce73d902915cb84b17f5906d29354fbb` | |
| V1b-2b | **SHA なし** | worktree `.worktrees/5a-v-v1b-2b`（branch `feat/5a-v-v1b-2b`）は `a8030f4d` のまま、**未 commit の変更**が `crates/evorch-core/src/admin_composition/`、`restart.rs`、`restart/` などにある。commit されるまで SHA を固定できない |
| 並行 harness 修正 | **SHA なし（取得していない）** | 計画 §6 の「編集禁止領域」に当たる変更の SHA は、この作業では調べていない |

- B3-01、02、04 の worktree（`.worktrees/d3a2-b3-01`、`d3a2-b3-02-pty`、`d3a2-b3-04-chromium`）は、
  作成時点で `1aa7d899` を指す。`d3a2-b3-01` には未 commit の変更がある（`process-guard/tests/lifecycle.rs`、
  `sandbox/src/ledger.rs`、`sandbox/tests/closed_stdio.rs`）。この作業では触っていない。
- 各担当は、編集前に main を更新し、実際の差分でファイル数を見積もり直すこと（計画 §3-1）。

## 2. 用語と「11 root」の定義（レビュー S1）

### 11 root の定義

**11 root ＝ 8 種の `ChildKind` ＋ StdioSession ＋ PTY ＋ js-worker。** 実 Chrome と CEF は、これとは
**別の追加セル**で、11 に数えない。

| 根拠 | 内容 |
|---|---|
| `run-01m7vsntp9g4djcxv88wh56aqc#245.2`（親への報告） | 「A-only 実10秒満了74後、**8kind＋StdioSession＋PTY の抵抗fixture 10/10 roots** が生存」 |
| 同 `#210.2`（設計 v2 の実測 6 の表） | 10 個の内訳は、`McpStdio`、`SandboxSession`、`PullRequestSource`、`ShellJob(pipe)`、`Eval`、`Probe`、`Renderer`、`Other`（以上 `GroupChild`）、「**LSP相当 direct StdioSession**」、「**PTY / portable_pty**」 |
| 同 `#390.3`（設計 v3 への js-worker の追加） | 「API matrix は従来10＋実 js-worker 1 の **11 root**。実 Chrome/CEF は追加セル」。「10 セルを消さず 11 root に増やす」 |

- 8 種の `ChildKind` は、`crates/sandbox/src/ledger/children.rs:11`：`McpStdio`、`SandboxSession`、
  `PullRequestSource`、`ShellJob`、`Eval`、`Probe`、`Renderer`、`Other`。
- 「StdioSession」は、設計 v2 の測定では、**ledger の種別ではなく、同期の stdio session を直接起動する経路**
  （LSP 相当）の root だった。**`StdioSession` 型は #750 で削除済み**で、今の同期 session の入口は
  `OwnedSession` である。**現在のコードのどの経路が「StdioSession の root」に当たるかは、履歴に明記がない。**
  表 1 では候補を示すだけで、**対応は親の承認事項**とする（§8-1）。
- 承認条件（承認 review の B3 節）：「実 release core の実満了で、11 個の root と同じ group の孫がすべて終わる」。

### 「13 synthetic 経路」との違い

`crates/sandbox/tests/owner_death.rs` の 13 root は、**API の入口**の網羅（11 root とは別の数え方）。
8 種の `ChildKind` ＋ 5 つの入口（`blocking-no-ledger`、`admission-spawn-owned`、`session-spawn-owned`、
`session-spawn-sandboxed`、`session-no-ledger`）で 13。**この 13 を 11 root や実 caller の証明として数えない**
（計画 C-8）。この試験の owner は `owner_death.rs` 自身を再 exec した**試験 binary**。

## 3. 表 1：承認条件の 11 root

「現在の実 caller」は表 3 の ID。「B3 での担当」は計画の PR 番号。

| # | root | 定義の根拠 | 現在の実 caller（表 3） | 合成試験での扱い（13 経路） | 実 caller の cell で必要な artifact | 状態・欠落 |
|---|---|---|---|---|---|---|
| 1 | `mcp-stdio`（`McpStdio`） | `children.rs:13` | R07 | `mcp-stdio` | 製品 ELF（config と protocol で MCP server を起動） | 合成 root のみ。実 caller の SIGKILL・満了は未実施（B3-06、07） |
| 2 | `sandbox-session`（`SandboxSession`） | `children.rs:15` | R05（comment-checker）、R06（LSP）、R08（js-worker。ただし #11 に数える）、R09（minimal の library 一覧）。ledger なしの verify は表 4 の L17 | `sandbox-session`、`session-spawn-owned`、`session-spawn-sandboxed` | 製品 ELF（agent の tool 経由）。bwrap と DirectSandbox の両方が要る（S2）。DirectSandbox は試験 binary | 同上 |
| 3 | `pull-request-source`（`PullRequestSource`） | `children.rs:17` | R15 | `pull-request-source` | 製品 ELF（PR area。外部ツール `gh` が要る） | 同上 |
| 4 | `shell-job`（`ShellJob`、pipe） | `children.rs:19`。#210.2 の「ShellJob(pipe)」 | R01 | `shell-job` | 製品 ELF（agent の `shell` tool。provider が要る） | 同上 |
| 5 | `eval`（`Eval`） | `children.rs:21` | R12 | `eval`、`admission-spawn-owned` | 製品 ELF ＋ 隣の `evorch` CLI（または PATH 上の `evorch`） | 同上 |
| 6 | `probe`（`Probe`） | `children.rs:23` | **なし** | `probe`（合成のみ） | 実 caller がない。**製品経路を作って埋めない**（計画 §3-2、C-8） | **欠落**。親へ提示（§8-2） |
| 7 | `renderer`（`Renderer`） | `children.rs:25` | R13 | `renderer` | 製品 ELF ＋ `evorch-renderer`（CEF）。fake helper は試験 binary | 同上。既存に core の owner が abrupt に終わる試験がある（`renderer_owner`、fake helper 使用） |
| 8 | `other`（`Other`） | `children.rs:26` | R03、R04、R10、R11、R16 | `other`、`blocking-no-ledger` | 製品 ELF（種別が多い。代表を選ぶ必要があり、**親の承認事項**） | 同上 |
| 9 | **StdioSession**（LSP 相当の同期 stdio session） | #210.2 | 候補：R06（LSP）。他に同期 session の経路 R09、R10 | `session-spawn-owned`、`session-spawn-sandboxed`、`session-no-ledger` | 製品 ELF ＋ LSP server の設定 | **対応が未承認**（§8-1）。#2 と同じ入口を共有する |
| 10 | **PTY** | #210.2 | R02 | **なし**（合成の 13 に PTY 経路は含まれない。raw backend のまま） | 製品 ELF（agent の `shell` tool の PTY 指定） | **B3-02、03 の前は typed 所有がない**。raw のまま |
| 11 | **js-worker** | #390.3 | R08 | **なし**（13 の外。静的な境界試験は B2 の計画だったが、この表では未確認） | 製品 ELF ＋ 隣の `evorch` CLI（または `EVORCH_JS_WORKER`）。bwrap 経路と、`WorkerLauncher::injected` の DirectSandbox（試験 binary）の両方 | 設計 v3 §6.2 の「bind しない」例外と「初期 exec 後に新しい process を作らない」確認が必要（B3-06） |

追加セル（11 に数えない）：

| 追加セル | 実 caller | 計画 | 備考 |
|---|---|---|---|
| 実 Chrome（agent Chromium） | R14 | B3-04、08 | 現在は chromiumoxide の内部 spawn（raw）。typed 所有がない |
| 実 renderer ＋ CEF の navigation 後の満了 | R13 | B3-08 | #7 とは別に、CEF を実際に初期化し navigation まで進める |

## 4. 表 2：API 網羅表（spawn の入口 API）

**API の入口を網羅する表。実 caller の証明ではない。** 定義位置は `crates/sandbox/src/` 配下（Linux）。
「13 経路の label」は `crates/sandbox/tests/owner_death.rs` の root。

| # | API | 定義 | 13 経路の label | 本番の呼出元（表 3、4 の ID） | メモ |
|---|---|---|---|---|---|
| A1 | `OwnedChild::spawn_blocking(ledger?, kind, label, cmd)` | `owned.rs:84` | 8 種の `ChildKind`（ledger あり）、`blocking-no-ledger`（ledger なし） | R01、R03、R04、R05、R11、R15、L17、L20 | 非同期の入口。blocking thread で admission・起動・登録 |
| A2 | `OwnedChild::spawn_in(&Admission, kind, label, cmd)` | `owned/linux.rs:326` | A1、A4 経由 | R07（MCP） | 直接の matrix 行なし |
| A3 | `OwnedChild::spawn(cmd)` | `owned/linux.rs:298` | なし | なし（本番の呼出元を確認していない） | ledger に登録されない |
| A4 | `Admission::spawn_owned(kind, label, cmd)` | `ledger.rs:304` | `admission-spawn-owned` | R12、R13 | A2 に委譲 |
| A5 | `OwnedSession::spawn(cmd)` | `owned/linux.rs:200` | `session-no-ledger` | L18（eval の git） | 同期。ledger なし |
| A6 | `OwnedSession::spawn_in(&Admission, …)` | `owned/linux.rs:205` | A7、A8 経由 | なし | |
| A7 | `OwnedSession::spawn_owned(ledger?, kind, label, cmd)` | `owned.rs:69` | `session-spawn-owned` | R09、R10、R16、L19 | |
| A8 | `OwnedSession::spawn_sandboxed(ledger?, sandbox, spec)` | `owned.rs:31` | `session-spawn-sandboxed` | R06（LSP）、R08（js-worker） | `ChildKind::SandboxSession` 固定 |
| A9 | `OwnedSession::spawn_sandboxed_in(&Admission, …)` | `owned.rs:44` | A8 経由 | なし | |
| R1 | `ProcessGroup::spawn_pty(slave, cmd)`（raw） | `process.rs:184` | **なし** | R02 | **B3-05 で撤去予定。typed の PTY 入口は B3-02、03 で作る** |
| R2 | `Admission::register_pty_group(kind, label, &group)`（raw） | `ledger.rs:275` | なし | R02 | 同上 |
| R3 | `ProcessGroup::retired()` | `process.rs:194` | なし | なし（試験の `m2.rs` が使う。計画 B3-05） | |
| R4 | `close_inherited_fds_on_exec` | `process.rs:80` | なし | なし（product-exec の試験が使う） | 所有 API ではない（README） |
| R5 | `ProcessGroupGuard`、`wait_for_exit`、`has_exited` | `process.rs:272`、`299`、`290` | なし | R02 | B3-05 で撤去予定 |
| X1 | `chromiumoxide::Browser::launch`（外部 crate の内部 spawn） | `browser/src/agent/chromium.rs:249` | なし | R14 | B3-04 で owned launch ＋ connect へ移す |

- 非 Linux には private な backend（`GroupChild`、`StdioSession::spawn_command_in`、`Admission::spawn_group`）が
  残る。**この表の対象外**（計画 B3-05：無理に同時撤去しない）。
- 13 経路のうち、A2、A3、A6、A9 に直接対応する label はない。A2、A9 は A4、A8 経由で通る。

## 5. 表 3：実 caller 表（実 evorch-core から届く本番の起動元）

**API 網羅表（表 2）とは別の表。** 実 core の呼出元だけを並べる。「ledger」は、core が作る単一の
`SpawnLedger`（`workbench-host/src/bootstrap/mod.rs:292`）が登録先になるか。
「既存の死亡・終了の試験」は README と test ファイルの先頭の記述から確認した範囲。

| ID | caller | 起動元（file:line） | ChildKind／11 root | API | ledger | stdio（stdin/out/err） | 子孫 | 既存の異常終了の試験 |
|---|---|---|---|---|---|---|---|---|
| R01 | shell の pipe job | `tools/src/tools/shell/jobs/process.rs:40` | ShellJob（#4） | A1 | あり（`job.ledger`） | null か pipe / pipe / pipe | `sh -c` と同 PGID の子 | `owner_death.rs` の `shell-job`（合成）のみ |
| R02 | shell の PTY job | 同 `:137–149`（`ProcessGroup::spawn_pty`、`register_pty_group`） | ShellJob の raw 登録（#10） | R1、R2（raw） | あり（raw group） | PTY | `sh -c` と同 PGID。interactive の job control で別 PGID に出た子は保証外 | 合成なし。既存の PTY 試験のみ（`process_tests.rs` 等） |
| R03 | grep（`rg`） | `tools/src/tools/grep.rs:115` | Other（#8） | A1 | あり（`self.ledger()`） | null / pipe / null | `rg` | 合成のみ |
| R04 | git diff tool | `tools/src/tools/git_diff.rs:139` | Other（#8） | A1 | あり | null / pipe / pipe | git と helper | 合成のみ |
| R05 | comment-checker（post-edit） | `tools/src/post_edit.rs:368` | SandboxSession（#2） | A1 | あり | pipe ×3 | checker | 合成のみ |
| R06 | LSP | `tools/src/lsp/client.rs:34` | SandboxSession（#2。#9 の候補） | A8 | あり（設定時） | pipe ×3 | server | 合成のみ。`--die-with-parent` と creator thread の関係は設計 v3 実測 1 |
| R07 | MCP stdio | `tools/src/mcp/transport/stdio.rs:668` | McpStdio（#1） | A2 | あり | pipe ×3 | server | `mcp_stdio`（ledger stop、README） |
| R08 | js-worker | `tools/src/tools/run_js/worker.rs:444` | SandboxSession（#11 に数える） | A8 | あり | pipe ×3 | worker（自身の子は作らない前提） | なし（B3-06 で新設） |
| R09 | minimal sandbox の library 一覧 | `sandbox/src/minimal.rs:404` | SandboxSession（#2） | A7 | あり（`detect_owned`） | null / pipe / pipe | 短命。一覧が終わると終了 | 合成のみ |
| R10 | bwrap の機能確認 | `sandbox/src/bwrap.rs:293` | Other（#8。#9 の候補） | A7 | あり（`detect_owned`）。ledger なしの呼出しもある | null / pipe / pipe | 短命（`true` を実行） | 合成のみ |
| R11 | codex の quota RPC | `providers/src/provider/codex/quota/rpc.rs:143` | Other（#8） | A1 | あり（`config.ledger`） | pipe / pipe / null | app-server | 合成のみ |
| R12 | eval の group member | `eval/src/evaluator/groups.rs:111` | Eval（#5） | A4 | あり | null / file / file | `evorch eval run` の process。その先の子は別の owner（表 4 の L17、L18） | `groups` の unit test（README） |
| R13 | offline renderer | `workbench-host/src/areas/browser/renderer.rs:545` | Renderer（#7） | A4 | あり | IPC endpoint ×2 / null | CEF の native 子（同 group と設計 v3 §6 は想定。実測は B3-08） | `renderer_owner`（fake helper。core の abrupt な終了）、`renderer_owner_cef`（実 CEF。test-only の subreaper 確認） |
| R14 | agent Chromium | `browser/src/agent/chromium.rs:249`。本番の `AgentBrowser::new` は `tools/src/executor.rs:701` の 1 箇所 | 追加セル（11 に数えない） | X1（外部 crate） | **なし** | chromiumoxide 任せ | Chromium の子孫。group の構成は未測定（B3-04 の最初に採る） | `agent_observation_chromium`、`agent_chromium` |
| R15 | PR source（`gh`） | `runtime/src/pull_requests/source.rs:232`。runner は `workbench-host/src/bootstrap/lane_pr.rs:30` | PullRequestSource（#3） | A1 | あり | null / pipe / pipe | `gh` | `pull_requests_core` ほか |
| R16 | git の読み取り（thread 開始時など） | `runtime/src/git_read.rs:151`。呼出元は `runtime/src/thread_start.rs:77`、`repository.rs:143` | Other（#8） | A7 | あり | null / pipe / pipe | git | 合成のみ |

- R01〜R16 のうち、**typed 所有になっていないのは R02（PTY）と R14（agent Chromium）**。B3-02〜04 の対象。
- core の ledger を設定する箇所は `bootstrap/mod.rs` の `with_spawn_ledger`、`set_spawn_ledger` 等。
  **「core では全 caller の ledger が Some」とまでは確認していない**（呼出元ごとの設定漏れは未調査）。

## 6. 表 4：spawn 台帳（全分類）

**行ごとの項目：** caller、owner process、ChildKind、ledger、起動 API、子孫、正常・異常時の終了、
**artifact 種別**、**到達する入口**（レビュー S2）。

### artifact 種別の記号（S2）

| 記号 | 意味 |
|---|---|
| **P** | 製品 ELF から、既存の入口（protocol、config、env、agent の tool 呼出し）だけで届く。実行環境の外部ツール（git、bwrap 等）以外の追加物は要らない |
| **P\*** | 製品 ELF から届くが、追加物が要る。隣の別 binary、外部ツール、provider、設定、display など（列「追加で必要なもの」） |
| **T** | 試験 binary、test seam、feature でしか届かない（`TestSeams`、`WorkerLauncher::injected`、`renderer-test-helper` など） |
| **X** | owner が core ではない、または製品の spawn ではない。B3 の core 死亡・満了の対象外（理由を書く） |
| **raw** | 現在は typed 所有ではない（B3 の移行対象）。種別の記号と併記する |

**判定はすべて読解（未実行）。** 「製品 ELF」は、sealed の `evorch-core-product`（`evorch-core`、guard、bundle、manifest の
4 ファイル）と、Nix の `evorch` package（`evorch` と `.evorch-core-wrapped` が同じ bin にある。`flake.nix:200`）を指す。
**この 2 つで届く範囲が違う**（`evorch` を隣に要する行は P\* とし、sealed の 4 ファイルだけでは届かない）。

### 6-A. core が所有する（B3-06 の対象候補）

| ID | caller（file:line） | owner | kind／ledger | API | 子孫・終了 | artifact | 到達する入口（読解） | 追加で必要なもの／試験 binary でしか届かない変種 |
|---|---|---|---|---|---|---|---|---|
| L01 | shell pipe（R01） | core | ShellJob／あり | A1 | `sh -c` と同 PGID の孫。通常は `kill_and_wait`（`process.rs:99`）。core の死は guard が KILL | P\* | agent の `shell` tool（agent run）。agent run は protocol で開始し、provider は config の `base_url`（`config/types/provider.rs:838`）で mock に向けられる | provider の設定と鍵、実 sandbox の bwrap（`bootstrap/components.rs:54`）。**DirectSandbox は `TestSeams` 経由＝T** |
| L02 | shell PTY（R02） | core | ShellJob（raw group）／あり | R1、R2 | root と同 PGID まで。別 PGID は保証外 | P\* ・**raw** | L01 と同じ tool の PTY 指定 | L01 と同じ。**未移行（B3-02、03）** |
| L03 | grep（R03） | core | Other／あり | A1 | `rg` | P\* | agent の `grep` tool | provider、`rg` |
| L04 | git diff tool（R04） | core | Other／あり | A1 | git | P\* | agent の git diff tool | provider、git |
| L05 | comment-checker（R05） | core | SandboxSession／あり | A1 | checker | P\* | edit、write、apply_patch の post-edit hook（`tools/src/tools/edit.rs:22` 等） | hook の構成と checker の実行ファイル。**構成の有無は未確認** |
| L06 | LSP（R06） | core | SandboxSession／あり | A8 | server | P\* | agent の LSP tool | provider、LSP server、bwrap（DirectSandbox は T） |
| L07 | MCP stdio（R07） | core | McpStdio／あり | A2 | server | P\* | `mcp` 名前空間の protocol command ＋ config（`runtime/src/mcp/service.rs:367`。`mcp_area_ops` が既存の入口の試験） | MCP server の実行ファイル（試験の `core-mcp-test-peer` は試験 bin。製品は任意の server を起動できる） |
| L08 | js-worker（R08） | core | SandboxSession／あり | A8 | worker（evorch js-worker）。**guard は core 側だけ** | P\* | agent の `run_js` tool。worker の場所は `EVORCH_JS_WORKER` か、current_exe の隣の `evorch`（`run_js/worker.rs:476`） | **`evorch` を隣に置くか env を設定**（sealed の 4 ファイルには `evorch` がない）。provider、bwrap。**注入した DirectSandbox（`WorkerLauncher::injected`）は T** |
| L09 | minimal sandbox の library 一覧（R09） | core | SandboxSession／あり | A7 | 短命 | P\* | L08 の準備中だけ（`worker.rs:406` の `detect_owned`） | L08 と同じ |
| L10 | bwrap の機能確認（R10） | core | Other／あり（ledger なしの呼出しもある） | A7 | 短命 | P | core 起動時の sandbox 検出（`bootstrap/components.rs:54`）ほか（`runtime/src/network.rs:76`、`bwrap/startup.rs:23`） | bwrap |
| L11 | codex の quota RPC（R11） | core | Other／あり | A1 | app-server | P\* | provider の quota 取得 | codex の app-server の実行ファイルと設定 |
| L12 | eval group member（R12） | core | Eval／あり | A4 | `evorch eval run`。その先は別の owner（L17、L18） | P\* | agent の eval tool（`bootstrap/mod.rs:390`）。CLI は `find_cli`（隣の `evorch`、なければ PATH。`groups.rs:40`） | `evorch` CLI、provider。**sealed の 4 ファイルだけでは隣に `evorch` がない（PATH で補える）** |
| L13 | offline renderer（R13） | core | Renderer／あり | A4 | CEF の native 子 | P\* | browser area の tab 操作（`areas/browser.rs:38`）。renderer の場所は `EVORCH_RENDERER_EXEC` か隣の `evorch-renderer`（`renderer.rs:94`） | `evorch-renderer`（CEF）、CEF の配布、display の要否は未確認。**fake helper（feature `renderer-test-helper`）は T** |
| L14 | agent Chromium（R14） | core | （kind なし）／**なし** | X1 | Chromium の子孫 | P\* ・**raw** | agent の browser tool（`tools/src/executor.rs:701`、`detect` に成功した場合のみ） | Chromium、provider。**未移行（B3-04）** |
| L15 | PR source（R15） | core | PullRequestSource／あり | A1 | `gh` | P\* | PR area の protocol query（`lane_pr.rs`） | `gh` と認証 |
| L16 | git 読み取り（R16） | core | Other／あり | A7 | git | P | thread 開始（`thread_start.rs:77`）、repository 情報（`repository.rs:143`）の protocol 操作 | git |

### 6-B. owner が core の直接の子ではない（owner の二段化）

| ID | caller | owner | kind／ledger | API | 子孫・終了 | artifact | 到達する入口 | メモ |
|---|---|---|---|---|---|---|---|---|
| L17 | eval の verify（`eval/src/verify.rs:84`） | `evorch eval run`（L12 の root） | SandboxSession／**なし** | A1 | root exit → 出力 drain → 固定 1 秒の reap（README） | X | L12 の子として届く | core の guard が L12 の root を KILL → L17 の guard は owner（eval run）の死で KILL する連鎖。**連鎖は未検証** |
| L18 | eval の git（`eval/src/git.rs:40,99`） | 同上 | なし／なし | A5 | git | X | 同上 | 同上 |
| L19 | snapshot の git（`runtime/src/snapshot.rs:165`） | **`evorch` CLI の benchmark**（`evorch/src/benchmark/record_run.rs:176`、`replay_run.rs:61`） | Other／なし（`SnapshotStore::open` は ledger なし） | A7 | git | X | core からは呼ばれない（`open_owned` の本番の呼出元がない） | 計画の旧記述「runtime snapshot」は core の実 caller ではない |

### 6-C. GUI が owner（core の保証の対象外）

| ID | caller（file:line） | owner | 起動 | 子孫・終了 | artifact | 分類 | メモ |
|---|---|---|---|---|---|---|---|
| L20 | GUI の slash command（`gui/src/model/slash_process.rs:38`、builder は `external_slash.rs:8,73`） | GUI | A1（ledger なし、kind は Other） | 子孫と同 group。timeout や drop で group ごと終了 | X | 移行済み（B2b-iii）。core の死では GUI 側の guard は動かない（owner が GUI） | |
| L21 | **GUI の埋め込み terminal の PTY**（`gui/src/pty/mod.rs:122`、`ShellSpawner` は `pty/mod.rs:80`、配線は `bin/evorch-gui.rs:1303`） | GUI | raw（`portable_pty` の `spawn_command`） | ユーザの shell。Drop で kill | X・**raw** | **未移行**。README の「未監査 spawn」の一覧にない（**新たに見つけた経路**）。親の判断 C により、台帳に載せて別の follow-up とする | 親へ提示（§8-4） |
| L22 | GUI の diff（`gui/src/diff/git_cli.rs:16`） | GUI | std `Command::output` | git | X | 未移行。短命の git | README に記載あり |
| L23 | GUI の composer mention（`gui/src/model/composer_mention.rs:500`） | GUI | std `Command` | git | X | 未移行。短命 | README に記載あり |
| L24 | GUI の demo repo（`gui/src/bin/evorch-gui.rs:335,368`） | GUI（`--demo` のみ） | std `Command::output` | git | X | 未移行。短命 | |
| L25 | GUI の `fc-match`（`gui/src/theme/fonts.rs:96`） | GUI | std `Command::output` | fc-match | X | 未移行。短命 | README に記載あり |
| L26 | GUI の in-process CEF（`browser-cef`） | GUI | CEF 内部の spawn（外部 crate／native） | CEF の子 | X | **未監査**。この作業では調べていない | 設計 v3 §6 の「外部 crate の内部 spawn の別監査」に当たる |

### 6-D. launcher と lifecycle

| ID | caller（file:line） | owner | 起動 | 分類 | メモ |
|---|---|---|---|---|---|
| L27 | core の起動（`core-launcher/src/lib.rs:231`） | GUI／CLI（launcher） | std `Command`（`launch.spawn`）。stdin/stdout は null、stderr は継承 | **意図的な対象外**（案） | core は launcher の所有物ではなく、独立した常駐 process として起動される（コード読解）。guard の対象とすると、launcher が終わるたびに core が死ぬ |
| L28 | private preflight supervisor（`core-launcher/src/handoff/supervisor.rs`。fork、`:306` PDEATHSIG(TERM)、`:327` setpgid、`:346` execve） | core（planned handoff の前の probe） | raw の fork（allocator なし） | **意図的な対象外** | 計画 B3-06：二重に guard しない。core の死・EOF・TERM・exec 失敗の既存保証を回帰確認する |
| L29 | planned exec（`core-instance/src/handoff.rs:514` の `execve`） | core 自身 | 自分の exec | **対象外（子を作らない）** | |

### 6-E. guard 自身

| ID | caller（file:line） | 起動 | 分類 | メモ |
|---|---|---|---|---|
| L30 | guard helper の起動（`process-guard/src/lib.rs:107`） | owner process から std `Command`（pin した image を `/proc/self/fd/N` で exec） | 対象外（guard の実装） | |
| L31 | target の fork と exec（`process-guard/src/guard.rs:354`、`:150`） | helper の中 | 対象外（guard の実装） | |
| L32 | build script（`process-guard/build.rs:3`、`rustc -V`） | build 時 | 対象外 | |

### 6-F. 試験専用

| ID | 内容 | 分類 |
|---|---|---|
| L33 | tests、test-support、examples、harness にある spawn：**124 ファイル**（grep による。`Command::new`、`OwnedCommand::new`、`OwnedChild::spawn`、`OwnedSession::spawn`、`spawn_owned`、`spawn_sandboxed`、`spawn_command`、`native_pty_system`、`libc::fork`、`ProcessGroup::spawn_pty` のいずれかを含む）。crate 別：evorch-core 30、runtime 29、tools 17、sandbox 11、gui 10、storage 6、evorch 6、core-launcher 4、workbench-host 3、process-guard 2、その他 5。加えて、`src` 内の `#[cfg(test)]` の中にある spawn（`storage`、`config/src/load.rs:541`、`tools/src/output.rs`、`tools/src/tools/run_js/*`、`core-instance/src/handoff_boot.rs:534`、`handoff_physical_deadline.rs:303`、`workbench-host/src/attachments.rs:1035`、`ingress/attachment_race.rs:187`、`sandbox/src/minimal.rs:743`、`evorch-core/src/lifecycle/idle.rs:79`、`gui/src/runtime_sink.rs:2226`、`gui/src/bin/evorch-gui.rs:1908,1927` など） | **対象外（試験）**。個々の行は分類していない。ファイル単位の grep なので、**行単位の網羅ではない** |

## 7. 台帳の要点

- **行数：** 個別の行は **32 行**（L01〜L32）。L33 は試験専用の集約 1 行（124 ファイル）。
- **artifact 種別ごとの件数（L01〜L32）：**

| 種別 | 件数 | 行 |
|---|---|---|
| P | 2 | L10、L16 |
| P\* | 14 | L01〜L09、L11〜L15 |
| X（owner が core でない、または製品の spawn でない） | 16 | L17〜L32 |
| T | 0 行 | ただし **P\* の行に T の変種が付く**：L01、L06（DirectSandbox）、L08（`WorkerLauncher::injected`）、L13（`renderer-test-helper`） |
| raw（未移行。上の種別と併記。X の行にも付く） | 3 | L02、L14（core の caller。B3 の対象）、L21（GUI の PTY。対象外） |

  - **typed 所有でない core の caller は L02、L14 の 2 行。** GUI にも typed でない起動がある：
    PTY が L21、std の `Command` が L22〜L25（4 行）。これらは X に数え、raw の件数（3）には L21 だけを入れた。
- **core 所有の本番 caller（表 3）：16 件（R01〜R16）。うち typed 所有でないのは 2 件（R02 PTY、R14 agent Chromium）。**
- **11 root との対応：** 実 caller がある root は 10 個。**`probe` は実 caller がない。** 同期 session（#9）の対応は未承認。
- **sealed の 4 ファイル（`evorch-core-product`）だけでは届かない実 caller：**
  - L08 js-worker と L09 の library 一覧（隣の `evorch` が要る。`EVORCH_JS_WORKER` で補える）
  - L12 eval（隣の `evorch` が要る。PATH で補える）
  - L13 renderer（隣の `evorch-renderer` が要る。`EVORCH_RENDERER_EXEC` で補える）
  - **補えるかどうかは、どれも実行して確かめていない。**
- **製品 ELF からも届かない、または届かない可能性がある実 caller（読解）：**
  - `Probe`：caller がない。
  - L19 snapshot の git：core の実 caller ではない（CLI の benchmark）。
  - L17、L18：core の直接の子ではない（L12 の子）。
  - **DirectSandbox を使う cell（L01、L06、L08 の変種）は、試験 binary でしか作れない。**
    製品 ELF の sandbox は実 bwrap（`bootstrap/components.rs:54`）。
- 製品 ELF の起動入口が実在するかを含め、**どの行も実行では確認していない**。
  provider を mock に向けられるのは config の `base_url` が根拠で、**製品 ELF が mock provider と鍵の取得経路で
  動くことは未検証**。

## 8. 親へ提示する事項（計画 §3-2、レビュー S1、S2）

1. **「StdioSession」root の対応。** 設計 v2 の「LSP 相当 direct StdioSession」は、型の削除後、現在のどの経路に
   当たるのか履歴に明記がない。候補は R06（LSP）と、同期 session の R09、R10。**承認が要る。**
   承認するまで、表 1 の #9 は「未承認」のまま。
2. **`Probe` には本番の caller がない。** 製品経路を作って埋めない。合成の root（`owner_death.rs`）で扱うか、
   11 root から外すかは親が決める。
3. **S2：製品 ELF で既存の入口から起動できない caller、または試験 binary でしか作れない cell。**
   - DirectSandbox の cell（L01、L06、L08 の変種）は試験 binary。B3-06 は、製品 ELF の cell と試験 binary の
     cell の**両方**を別々に提出し、artifact の種別を各行に書く必要がある。
   - sealed の 4 ファイルに隣の binary が要る caller は、上の §7 のとおり。expiry と同じ扱い（親の判断が先）。
4. **新たに見つけた未移行の経路：GUI の埋め込み terminal PTY（L21）。** README の未移行の一覧になかった。
   親の判断 C に従い、台帳に載せ、別の follow-up とする。GUI が owner なので、core の SIGKILL・満了の対象外。
5. **README の「runtime snapshot」の扱い。** L19 は core の caller ではなく、CLI の benchmark から呼ばれる。
   B3-05 の台帳では「core 所有」に入れない。
6. **core が作る単一の ledger が全 caller に渡っているか**は未確認（表 3 の注）。B3-06 の前に確認する。

## 9. 未実施・未確認

- 計画 §3-3（expiry 入口の実現性調査）と §3-4（測定条件の固定）は、この作業では行っていない。
- どの行も実行していない。到達の判断はコード読解で、provider の mock 化、bwrap の有無、CEF の配布、display の要否は
  実環境で確かめていない。
- 並行 harness 修正の SHA と、V1b-2b の SHA は取得できていない（§1）。
- 外部 crate と native の内部 spawn（chromiumoxide の内側、CEF の内部）の監査はしていない（L14、L26）。
- 試験専用の spawn（L33）は、ファイル単位の grep で、行単位の分類ではない。
