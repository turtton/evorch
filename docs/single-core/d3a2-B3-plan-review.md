## 判定：承認（must なし）

対象は `docs/single-core/d3a2-B3-plan.md`（commit `c1385bdd`、planner `run-01m7ypc53z68p1xzpktc5dwrat`）です。B3-01、02、04 と着手前作業は始めて構いません。B3-06 は、spawn 台帳を親が承認してから着手してください（S1、S2）。

読み取りだけで確認しました。build と試験は実行していません。

## 見てほしい点への答え

| 点 | 結果 |
|---|---|
| 受け入れ条件の網羅 | B3 節の 7 項目すべてに担当 PR がある。計画に明記が無い細部が 4 件（S1〜S4）。 |
| 分割・依存・境界 | 9 本の分け方と依存順は妥当。02 と 03 の間の API、05 と 06 の fixture の重なりは未固定（S5、S10）。 |
| 並行作業との衝突 | harness 修正の対象とも V1b-2b の未 commit 差分とも、path は重ならない。挙動への依存は残る（S10）。 |
| 負例・mutation・性能 | 負例と mutation は十分。性能は scaffold の 3 項目が抜け、B3 が変える PTY と Chromium の起動は gate の外（S4、S7）。 |
| 親の判断点 | A〜D は明確。足りないのは 2 件（S2、S5）。 |

計画書の事実記述は、次を実コードで確かめました。
- watchdog composition は試験 binary のコピーを使う（`planned_shutdown_watchdog_composition/parent.rs:38`）。
- target は無条件に `setsid` する（`process-guard/src/guard.rs:129`）。
- `ChildKind::Probe` に製品 caller は無い。
- `ledger.rs:267` の doc link は壊れている。

## should

### 仕様網羅

**S1. 「11 個の root」の定義が repo に無い**
- 根拠：設計 v3（`d3a2-B-design-v3.md`）と scope の TSV は `$TMPDIR` から消えていて、repo にもありません。
- thread の履歴では、11 は「8 種の ChildKind、StdioSession、PTY、js-worker」で、実 Chrome と CEF は追加のセルです（`run-01m7vsntp9g4djcxv88wh56aqc#245.2`、`#390.3`）。
- 影響：B3-06、07 の対応表と最終受け入れの基準が、読む人によって変わります。
- 修正：着手前作業 2 の表にこの定義を書いて repo に保存してください。approval review が参照する §6.1 と §8.2 も、必要な部分を履歴から復元してください。

**S2. B3-06 の「実 core」が、製品 ELF か試験 binary か書かれていない**
- 根拠：owned README は "real release core running its real callers" としています。計画で artifact の明示を求めているのは B3-07 だけです。
- DirectSandbox の注入は試験用の入口でしかできないので、B3-06 には 2 種類の artifact が混ざります。
- 影響：最大の PR を試験 binary だけで組むと、最後に差し戻されます。
- 修正：台帳に「artifact 種別」と「到達する入口」の列を足してください。製品 ELF で既存の入口から起動できない実 caller は、expiry と同じ扱いで親に返してください。

**S3. Chromium の要件のうち、計画に明記が無いもの**
- endpoint が loopback であることの確認（approval review の Chromium の項）。計画は「不正 endpoint」としか書いていません。
- agent Chromium が 74 の後に終わること。B3-08 の完了条件で 74 に触れているのは、renderer と CEF の文だけです。
- HandlerConfig の全 field。現行は `respect_https_errors()` で証明書の誤りを拒みます（`chromium.rs:215`）。`HandlerConfig::default()` は `ignore_https_errors: true` です（chromiumoxide 0.9.1 `handler/mod.rs` の Default 実装）。
- 影響：default を使うと、不正な証明書を黙って受け入れます。
- 修正：C-13 の値を試験で直接 assert してください。旧 launch の HandlerConfig は外から観測できないので、比較ではなく期待値の固定になります。

**S4. 性能の条件のうち、計画 §5 に無いもの**
- restart README の scaffold にあって §5 に無いのは、force kill の 200 sample 以上、stress-ng の必須、bind と hash の起動費用の別報告です。
- gate の workload は非 interactive で pipe の経路を通ります。B3 が変える PTY job の起動（`jobs.rs:304`）と Chromium の起動は測られません。
- 修正：PTY job の起動 RTT を A/B で記録してください。報告だけで構いません。閾値が要るなら、測る前に親が決めます。

### 設計

**S5. process-guard への依存追加と、PTY の起動方式が判断事項に無い**
- 根拠：v3 の scope は「process-guard の追加 dep は実装前に追加承認」としていました（同 run `#319.4`）。計画 B3-02 は「通常依存にする場合は receipt も更新」とだけ書いています。
- restart README の DB-26 addendum は、locator を見直す時期として B3 を挙げています。計画は「無断で変えない」としていますが、親への確認事項にはなっていません。
- 修正：判断事項に次の 2 つを足してください。
  - portable-pty を process-guard の通常依存にしてよいか。
  - locator は B3 で見直さない、という確認。
- あわせて、誰が openpty し、master と slave を誰が持つかを、B3-02 の PR 本文で固定してください。03 はその API を使います。

**S6. agent Chromium の所有と後始末の順序**
- ledger：`AgentBrowser::new` の呼び出しは 3 か所あります（`tools/src/executor.rs:701`、`runtime/src/runtime.rs:5160`、`gui/src/browser/agent.rs:397`）。`runtime.rs` は中央ファイルです。
  - `LaunchOptions` に ledger を足すと、B3-04 の範囲を越えます。
  - ledger に登録しないことと、planned exec をまたぐときの扱いを README に書いてください。`agent_observation_restart` を回帰に入れてください。
- profile の削除：`Running` は `_browser` から `_profile` の順に drop されます（`chromium.rs:57-64`）。owned child の Drop は control を half-close して reaper thread に渡すだけです。
  - 影響：Chromium が生きている間に TempDir を消しにいくので、完了条件の「profile が残らない」を満たせません。
  - 修正：reap の確認後に消す所有構造にしてください。async worker を上限なしで塞がないでください。

**S7. 未検証の前提は、先に測る**
- 実 Chromium の process tree と PGID の trace は、B3-08 ではなく B3-04 の最初に採ってください。group を抜ける子があれば、実装前に親に返します。
- pipe の経路は #744 から main にあります。条件を固定した直後に、現 main で試し測定をしてください。合否には使いません。A/B の分離方法と閾値が成り立つかを、B3-09 より前に知るためです。

**S8. 非 Linux の PTY**
- 根拠：`run_pty` は全 OS で `ProcessGroup::spawn_pty` を使います（`tools/src/tools/shell/jobs/process.rs:149`）。
- 影響：B3-03 で非 Linux 用の adapter（`owned/unix.rs` と同じ形）を用意しないと、B3-05 で公開 API を撤去できないか、darwin の build が壊れます。darwin は CI の lint でしか確かめられません。

### テスト・規約

**S9. B3-02 に足す負例**
- helper が session leader になるので、master を閉じると kernel の SIGHUP が helper に届きます。現行の helper は受けた signal を group に転送します（`guard.rs:176` と reactor の signalfd の分岐）。
- 足す負例は 2 つです。
  - master を先に閉じた場合。
  - foreground の設定に失敗したら、exec せずに SpawnError で閉じること。

**S10. 並行作業との結合**
- B3-06〜08 は実 core を mock provider で動かします。harness 修正が merge されると挙動が変わりえます。assert は process の事実（pidfd、exit、READY）に絞り、merge 後に再実行してください。
- `composed_core.rs` は V1b-2b の worktree でまだ未追跡です。B3-06 の依存に「これを使う部分は V1b-2b の merge 後」と書いてください。今は 07 にしか書かれていません。
- 05 の `product_exec/process.rs` の編集と、06 の fixture は同じ directory です。直列にしてください。
- 中央ファイルを触る PR の branch 名は、integration の scope か scope 外にしてください（`scripts/ci-scope-gate.sh`）。
- 5a-v-plan §7 は、B3 の merge 後に V-deny、V-exec、V-cache を再実行するとしています。B3-09 の引き渡し事項に入れてください。

## 合意済みの設計制約

DB-1〜DB-27（差し替え後）は、そのまま有効です。下は B3 の計画に固有の分です。

| ID | 範囲 | 制約 | 根拠 | 検証 |
|---|---|---|---|---|
| C-1 | 全 PR | DB-1〜DB-27 を緩めない。 | approval review、計画 §2 | 各 PR の対応表 |
| C-2 | 分割と順序 | 9 本。順は 01、02、03、05 と、01、04。03 と 04 の後に 06、その後に 07。08 は 04 と 06 の後で、expiry の部分は 07 の後。09 は 05、07、08 の後。各 PR は既存の動作を壊さず、未適用の経路を README と PR 本文に書く。 | 計画 §4、§6、DB-25 | PR 本文、README の diff |
| C-3 | 編集禁止 | runtime の `meta/runs/waiting*`、`agent_loop*`、`rules`、`compaction`、`event-bus/src/event.rs`、`storage/src/projection.rs`、`evorch/src/cli/inspect.rs`、`gui/src/fixture/events.rs`、`gui/src/theme/tokens.rs` を変更しない。避けられなければ差分を親に示して止まる。 | 親の決定、計画 §6 | `git diff --name-only` |
| C-4 | V1b-2b | `admin_composition/restart` と製品の finalization を変更しない。`m2.rs` の変更は、V1b-2b から引き継いだ後に直列で行う。`composed_core` を二重に実装しない。 | 親の決定、計画 §6 | diff、着手時の SHA |
| C-5 | 中央ファイル | `Cargo.lock`、各 manifest、CI の編集は、merge 窓の管理者 1 人を通して直列にする。 | 計画 §6、`scripts/central-files.txt` | scope-gate |
| C-6 | expiry | 製品に hook を足さず、時計を短くしない。まず既存の入口で capture と実 10 秒満了が成立するかを調べる。不可なら、到達できない境界をコード位置付きで親に返す。親が選ぶまで未完の gate とする。artifact の種別を証拠に書き、試験 binary を製品 ELF の証拠と呼ばない。 | 親の決定（question-efb45d3c…）、計画 §1、§7A | 調査報告、B3-07 の PR 本文 |
| C-7 | A の tail | 満了の tail に cleanup、logger、allocation、lock、ledger や guard の呼び出しを足さない。10 秒を変えない。 | DB-12、restart README | diff、A の 34 セル |
| C-8 | spawn 台帳 | 3 つの表（承認条件の 11 root、B2b-iii の 13 経路、実 core から届く実 caller）を分ける。13 経路を実 caller の証明に数えない。caller の無い `Probe` を架空の経路で埋めない。対応は親が承認する。 | 計画 §3、§7B、owned README | 親の承認の記録 |
| C-9 | B3-01 | Rust の起動後に 0/1/2 を閉じ、`EBADF` を確かめてから API を呼ぶ試験を置く。target での `EBADF` の保持を新しい契約にしない。修正が不要なら、そう書く。 | #750 のレビュー、owned README、計画 | 試験と mutation |
| C-10 | B3-02 | helper が session leader で、controlling terminal を持つ。target は同じ SID、別の PGID で foreground。foreground を設定してから exec する。PTY では target の `setsid` を使わない。認証、Ready の絶対 5 秒、Commit 前の fork 禁止、image の pin は direct と同じ。portable-pty に hook を足さず、patch しない。任意の launcher callback を公開しない。 | DB-11、DB-17、DB-26、計画 | PTY のセルと mutation |
| C-11 | PTY の locator | `/proc/<core-pid>/fd/N` だけを使う。成立しなければ失敗で閉じる。pathname を開き直さない。PID 再利用の残余リスクを README から消さない。 | 親の決定（question-eb7873a7…、question-eecb6ad6…）、restart README | image と PTY の回帰 |
| C-12 | B3-03 | admission、openpty、起動、登録を 1 つの所有操作にする。途中で放棄されても、完了後の RAII で回収する。resize、EIO から EOF、EOT、終了コード、job timeout の意味を変えない。保証は root と同じ PGID まで。別 PGID は観測して回収する。 | DB-9、DB-11、計画 | PTY のセル |
| C-13 | B3-04 | browser が owned child として起動し、chromiumoxide は接続だけに使う。guard を実行 file の前置きにしない。argv、env、cwd、stdio（stdin 継承、stdout null、stderr pipe）を今と一致させる。HandlerConfig は `ignore_https_errors=false`、`ignore_invalid_messages=true`、`viewport=None`、`context_ids` 空、`request_timeout` 30 秒、`request_intercept=false`、`cache_enabled=true`。正規化してよいのは異なる key の順序だけ。browser 全体の close を足さない。外部 crate を patch しない。 | DB-21、計画、`chromium.rs:206-217` | golden、field の assert、browser-e2e |
| C-14 | endpoint | ExecOk の後、絶対 20 秒、総量 256 KiB、1 行 16 KiB。loopback 以外、EOF、不正な UTF-8、上限超過は失敗。成功後は stderr の reader を閉じ、drain の task を足さない。 | approval review、計画 | parser の負例と mutation |
| C-15 | B3-05 | Linux の公開 API から `spawn_pty`、`retired()`、`ProcessGroupGuard`、`wait_for_exit`、`has_exited`、`register_pty_group` を外す。public な偽 receipt の API を新設しない。`m2.rs` は「未確認の reap が finalization を止める」目的を保つ。 | #750 の本文、owned README、計画 | 境界検査、restart 試験の mutation |
| C-16 | B3-06 | 実 core が実 caller を起動し、READY の後に core だけを SIGKILL する。root と同じ PGID の孫の終了を pidfd などで観測する。bwrap と DirectSandbox の両方で行う。`--die-with-parent` だけで消えるセルを guard の証明に使わない。js-worker は実 worker の入口を使い、EOF だけで終わる偽陽性を避ける。preflight supervisor を二重に guard しない。 | owned README、DB-13、DB-22、計画 | matrix と mutation |
| C-17 | B3-08 | agent Chromium と、renderer と CEF を別の経路として証明する。それぞれ navigation の後に SIGKILL と 74 を行う。O-V1、O-V2、#714 を回帰する。process tree、SID、PGID を記録する。環境不足を skip の成功にしない。 | approval review の B3 節、計画 | CI の job、trace |
| C-18 | 試験 | 同期は READY、ExecOk、root の終了、FinalStatus、pidfd で行う。sleep と `kill(pid, 0)` を終了の証拠にしない。mutation は試験用の変更で行い、製品に fault 用の env や設定を足さない。「起動した後に保証が破れて失敗する」ことを確かめる。外側の controller が全 process を回収する。 | DB-14、計画 §2 | mutation の記録 |
| C-19 | 性能 | 条件、閾値、A/B の SHA は測る前に固定する。値は計画 §5 のとおり。超えたら条件を緩めずに再レビューする。CI の assertion にしない。製品に guard を外す flag を足さない。core の状態は通常の API から作る。 | DB-24、restart README、計画 §5 | 条件 file の SHA-256、生 data |
| C-20 | 完了 | B3 の PR が全部 merge されても、D-7 の完了としない。A と B3 の証拠を揃え、独立 Reviewer と親が受け入れた後に、5a-V の公開列車に進む。 | DB-25、J8、計画 §1 | 受け入れの記録 |
| C-21 | cache | provider の入力、tool の schema、description、順序を変えない。`check-cache-contracts.sh` を弱めない。 | AGENTS.md、計画 B3-09 | script |

## 提案（合意前。採否は親の判断）

| ID | 内容 | 元 |
|---|---|---|
| P-1 | 11 root の定義（8 種、StdioSession、PTY、js-worker）を台帳に書いて保存する。 | S1 |
| P-2 | 台帳に artifact 種別と到達する入口の列を足す。製品 ELF で届かない行は親に返す。 | S2 |
| P-3 | process-guard の通常依存の追加は、実装前に親が承認する。locator を B3 で見直さないことも確認する。 | S5 |
| P-4 | Chromium の trace と、性能の試し測定を先に行う。 | S7 |
| P-5 | PTY job の起動 RTT、force kill の 200 sample 以上、stress-ng の必須、bind と hash の費用を、測定条件に足す。 | S4 |
| P-6 | expiry の第 3 案：B3 は試験 binary で閉じ、製品 ELF での実満了を公開列車の merge 条件に足す。J8 を保ったまま、公開が main に入る前に製品 ELF の証拠を得られる。 | 計画 §7A |

## 未解決の質問

- expiry の調査は「不可」で戻る見込みです（未確認）。満了は不可逆 capture を要します。5a-v-plan §2.4 は、公開前の製品 ELF に管理操作を有効にする入口が無いとしています。案 1、案 2、P-6 のどれにするか、早めに決める準備をしてください。
- DB-27 は「`--describe-handoff` は bind しない」としていますが、owned README は bind を要するとしています。計画のリスク 6 が挙げているとおりで、B2 でどう承認されたかは確認していません。

## 未確認

- 設計 v3 の本体は読めていません。11 root の内訳と、依存追加の事前承認は、thread の履歴の断片に基づきます。
- 各 PR のファイル数の見積もりは、検証していません。
- chromiumoxide の接続側が loopback をどう扱うかは、見ていません。