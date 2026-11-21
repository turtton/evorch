## 判定：承認

B の設計 v3 を承認します。B1 から実装を始めて構いません。条件は、下の受け入れ条件と制約を brief に入れることです。build と試験は実行していません。

## MB1〜MB4 の確認

| 指摘 | 状況 | 確認した箇所 |
|---|---|---|
| MB1 経路の漏れ | 解消 | §6 の表に、agent Chromium、GUI の slash、preflight supervisor、js-worker が入りました。外部 crate の内部 spawn を別に監査する、とも書かれています。core 側の crate に、他の外部 spawn は見つかりませんでした（`workbench-web` の chromiumoxide は dev-dependency です）。 |
| MB2 kill の契機と FinalStatus | 解消 | §3.1：root の終了だけでは kill しません。契機は、wait の要求と root の終了、ForceKill、half-close、core の死です。FinalStatus は root の reap の直後に返します。subreaper はやめ、wait は target の PID だけです。 |
| MB3 発見と配布 | 解消 | §5：発見の順序、3 つの package、試験と CI への受け渡し、fd の pin、I0 の schema 2 が決まりました。新しい懸念が 3 つあります（下の should）。 |
| MB4 kernel の下限 | 解消 | §1：fallback を外し、5.11 未満は失敗で閉じます。 |

## 特に見てほしい 4 点

- **Chromium の後始末は、開示された差分を除いて同じです。**
  - chromiumoxide 0.9.1 の `launch` と `connect_with_config` を読み比べました。差は、子 process の所有と、`config` を持たないことだけです。`config` を使うのは incognito の判定だけで、今は使っていません。
  - 初期化の失敗と drop は、root だけの kill から group の kill に強くなります。
  - 新しく入る差分は、endpoint 読み取りの上限と loopback の確認です。どちらも資料に明記されています。
  - 足りない点が 2 つあります。今のコードに browser 全体の明示 close はありません（`crates/browser/src/agent/chromium.rs:58` の `_browser` は未使用です）。§6.1 の「明示 close」は新設になるので、B3 では足さないでください。stdio の扱い（stdin は継承、stdout は null、stderr は pipe）も、一致を確かめる対象に入れてください。DB-21 に入れました。
- **B1 と B2 の中間状態は安全です。**
  - B1 は呼び出し側を変えません。B2 は API の変更と、PTY 以外の全経路の移行を同じ PR で終えます。
  - B2 の後に残る未適用の経路は、PTY と agent Chromium です。README と PR に書く、とされています。
  - B2 から、3 つの入口は起動時に bundle を必要とします。配布は B1 が先に済ませるので、順序は合っています。
- **js-worker の例外は妥当です。**
  - js-worker は target 側で、sandbox の中に bundle はありません。入口は logging より前に分岐します（`crates/evorch/src/main.rs:7`）。
  - 守りは 3 重です。静的な境界試験、B3 の実 worker のセル、helper がなければ spawn が失敗することです。
- **保証の境界は正しく書かれています。** 足す点が 1 つあります。wait を一度始めると、取り消しても root の終了時に group が止まります（下の should）。

## should（承認の妨げにならない）

- **PTY の経路では、pin した fd から exec できない可能性があります。**
  - §5.2 は、継承した `/proc/self/fd/N` を exec するとしています。
  - portable-pty 0.9.0 は、exec の前に 2 より大きい fd をすべて閉じます（`unix.rs:152, 276`）。
  - B1 の exec の契約に関わるので、B1 の最初に probe で確かめてください。DB-26 に入れました。
- **起動時の bind の費用が測られていません。**
  - 入口は、自分の ELF の hash を計算して bundle と照合します。製品の core は約 123 MB です。
  - この費用は、planned successor の起動区間にも乗ります。F4-P は、この区間の期限を延ばさないとしています。§8.2 の gate には入っていません。
  - B2 の受け入れに実測を入れました。既存の running ELF の hash を再利用できるかも見てください。
- **compat ID の入力に「lock receipt」があります。**
  - build script が package の外の file を読むと、新しい worktree のたびに再 build になります（AGENTS.md の開発環境の節）。
  - process-guard は sandbox の依存になるので、影響は workspace のほぼ全体です。DB-23 に入れました。
- **wait の取り消し**：WaitRequested は sticky です。今は、取り消した wait は何も残しません。挙動の変更として README に書き、呼び出し側を監査してください。shell の pipe は毎周 wait を作り直すので、実害はないはずです。
- **開発の手順が変わります。** B2 から、入口の binary を build するたびに bind が要ります。AGENTS.md の開発環境の節の更新は main への別変更で、scope の表にありません。
- **CI の待ち時間**：全 shard が製品 artifact の job を待つことになります。増分を記録してください。
- **権限の規則**：「helper に書き込み bit なし」が所有者の書き込みも拒むなら、Nix 以外の通常の配置（0755）は通りません。どちらの意図かを明記してください。

## 受け入れ条件

### B1（helper と配布）

- 設計 §8.1 の B1 の行がすべて通る。
- 3 つの package すべてで、実 ELF の隣の bundle から helper を pin して起動できる。
- CI の全 group と全 shard に bundle が届き、最小の spawn が通る。inventory の試験 ID の集合は変わらない。
- I0 の manifest は schema 2 になる。core と helper のどちらの破損も拒否し、schema 1 も拒否する。F4-P の fixture は 4 file の closure を使う。
- 既存の子の起動経路に diff がなく、既存の suite が通る。
- PTY の経路でも pin した image から exec できることを、最初に probe で確かめる。できなければ、契約を直してから進む。
- compat ID は package の中の file だけで決まる。
- CI 時間の増分を記録する。README には「B 未適用、D-7 未完了」と書く。

### B2（PTY 以外の移行）

- PTY 以外の全経路が typed API を通る。Linux では、raw の Command を所有 API に渡すと compile が通らない。
- 既存の意味の回帰が通る。
  - verify の 2 段（終了、出力の排出、1 秒以内の reap）
  - session の `try_wait`、`output`、Drop
  - status と errno の golden
  - 取り消した後の再 wait
- creator thread が終わっても、bwrap と direct の子が死なない。LSP を含む。
- 8 種と ledger なしの経路で、core の SIGKILL の後に root と同じ group の孫が終わる。
- eval は、通常の Stop が TERM のまま、core の死では KILL になる。
- js-worker の境界試験と、その mutation が通る。
- 3 つの入口の bind は、欠落と不一致で失敗し、env の上書きを無視する。
- bind の所要時間と、planned 起動区間への影響を実測して報告する。基準は、測る前に親が決める。
- cache の契約と A の 34 セルが通る。
- 旧 PTY の経路に、PTY 以外から到達できない。
- README に未適用の経路（PTY、agent Chromium）を書き、D-7 の完了を主張しない。

### B3（PTY、Chromium、結合）

- 実 release core の実満了で、11 個の root と同じ group の孫がすべて終わる。確認は終了 event で行う。guard を外すと失敗する。
- 実 Chromium で次が通る。
  - argv、env、stdio、config の一致
  - endpoint 読み取りの上限
  - drop、初期化の失敗、状態の reset の後始末
  - core の 74 の後の終了
  - O-V1、O-V2、#714 の browser-e2e
  - group の trace の報告
- 実 renderer と CEF で、navigation の後の満了でも終わる。
- PTY で次が通る。
  - `sh -c` の root と同じ group の孫が終わる。
  - tty、foreground、resize、Ctrl-C、EOF が今と同じ。
  - 別 PGID の子は、境界として観測して回収する。
- preflight supervisor の既存のセルが通る。
- 性能の gate を満たす。条件と閾値は事前に固定したもので、生の data を残す。
- README に保証と保証外を書く。D-7 の完了は、A と B3 の merge の後に Reviewer と親が判断する。

## 実装に渡す制約

DB-1〜DB-8、DB-10〜DB-12、DB-14 は、前回の文面のまま有効です。DB-9 と DB-13 を差し替え、DB-15 以降を追加します。

| ID | 範囲 | 制約 | 根拠 | 検証 |
|---|---|---|---|---|
| DB-9（差し替え） | 既存の意味 | `exited()` は root の終了で、kill を予約しない。`wait()` は root の reap の後に元の status を返す。kill は要求で、完了は wait で確かめる。既存の上限を変えず、新しい grace timer を足さない。取り消しても status を失わない。wait を始めた後の取り消しでも kill の予約が残ることを README に書く。 | §3.1、should | verify、session、取り消しのセル |
| DB-13（差し替え） | 適用範囲 | §6 の表の全経路に適用する。ledger の有無と guard の有無を結び付けない。外部 crate の内部 spawn も監査する。新しい経路が見つかったら、表と scope を更新して親に確認する。既存の `--die-with-parent` と preflight supervisor は残す。 | §6 | 表、11 root の matrix |
| DB-15 | kill の契機 | group の kill は、wait の要求と root の終了、ForceKill、half-close、core の死のときだけ。root の終了だけでは kill も reap もしない。 | §3.1 | verify のセル |
| DB-16 | FinalStatus | 条件は、group への kill の実行と root の reap の 2 つ。子孫の終了や回収を待たない。guard は subreaper にならず、wait するのは target の PID だけ。 | §3 | zombie がたまらないセル |
| DB-17 | transport | 認証の根拠は、相手の PID と UID の照合。nonce と address を秘密として扱わない。handshake は Ready の受信まで絶対 5 秒で、受信のたびに延ばさない。target の fork は Commit の後だけ。 | §4.1、§4.2 | handshake のセル |
| DB-18 | helper の env | helper は env を消して起動する。target の env と cwd は、認証後の LaunchSpec で渡し、target の exec だけに当てる。 | §4.3 | `LD_PRELOAD` の負例 |
| DB-19 | 発見 | 入口の binary は、実 ELF の隣の bundle だけを使う。library と試験は、渡された runtime か、起動時の環境変数だけを使う。PATH、cwd、project の設定、target の env は見ない。見つからなければ失敗し、直接の起動に切り替えない。 | §5.1 | delivery のセル |
| DB-20 | kernel | fallback を足さない。5.11 未満と、拒否された syscall は失敗で閉じる。 | 親の決定、§1 | diff |
| DB-21 | Chromium | browser が所有して起動し、chromiumoxide は接続だけに使う。argv、env、cwd、stdio、config は今と一致させる。正規化してよいのは key の間の順序だけ。実行 file の正規化も今と同じにする。browser 全体の明示 close を新設しない。外部 crate を patch しない。 | 親の決定、§6.1 | golden、browser-e2e |
| DB-22 | js-worker | js-worker は bind しない。入口の分岐は logging より前のまま。子 process を起動しないことを静的に確かめる。前提を変えるときは、先にレビューする。 | 親の決定、§6.2 | 境界試験と mutation |
| DB-23 | compat ID | 入力は process-guard の package の中の file だけ。build 時に `Cargo.lock` や他の crate を読まない。feature で wire や実装を切り替えない。 | AGENTS.md、§5.1 | 新しい worktree での再 build の確認 |
| DB-24 | 性能 | 条件と閾値は、測る前に固定する。後から変えない。超えたら合格にせず、再レビューする。CI の assertion にしない。 | 親の決定、§8.2 | 条件の file と生の data |
| DB-25 | PR | B1、B2、B3 の順に進める。各 PR は既存の動作を壊さない。未適用の経路を README と PR に書く。D-7 の完了は、A と B3 の merge の後だけ。 | 親の決定、§7 | 各 PR の記述 |
| DB-26 | image からの exec | 全経路で、pin した image から exec する。PTY の経路を含む。pathname を開き直さない。 | §5.2、portable-pty の挙動 | B1 の probe、pin のセル |
| DB-27 | bind の位置 | core の bind は、signal の mask と inherited FD の検証の後、config、DB、runtime の前。開く fd は atomic な CLOEXEC。`--help` と `--describe-handoff` は bind しない。 | §5.1、`core-instance/src/handoff.rs:374` | `planned_exec` の回帰 |

## 未確認

- 支える最も古い kernel（5.11）での動作。CI にその環境はありません。
- 実 Chromium と CEF の、子孫の group の構成。B3 で trace を採る前提です。
- nextest の metadata を再利用する流れが、全 group で再 build を起こさないこと。B1 で確かめてください。