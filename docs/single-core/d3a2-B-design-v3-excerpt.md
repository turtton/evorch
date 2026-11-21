# D-3a2 B 設計 v3 — 復元抜粋（§6、§6.1〜§6.3、§8.1、§8.2）

この文書は、`d3a2-B-v3-approval-review.md`（承認 review）が参照している設計 v3 の
§6.1（agent Chromium の具体方式）と §8.2（完成品性能 gate）、および両節の周辺を、
**thread の履歴から復元した**ものです。推測で補った箇所はありません。

## 出典と復元方法

- 設計 v3 の原本は `$TMPDIR/d3a2-B-design-v3.md`（thread-9gkyvvvf の scratch）でした。
  現在は消えていて、repo にもありません。最後の SHA256 は履歴に
  `a9908c0a52e7f9e91496966beac32646a806c93bb051d36845f0be7205fcdc65` と残っています
  （`run-01m7vsntp9g4djcxv88wh56aqc#396.0`、`#398.2`、`#400.1`）。
- 設計 v3 を書いた run は `run-01m7vsntp9g4djcxv88wh56aqc` です。節ごとに、その run が
  `run_js` で原本へ追記した文字列と、あとから適用した置換を、履歴から読み出しました。

| 節 | 最初の追記（`run-01m7vsntp9g4djcxv88wh56aqc` の ref） | 反映した後の置換（同 run の ref） |
|---|---|---|
| §6、§6.1、§6.2（旧。現 §6.3） | #319.4 | #344.2、#371.3、#390.3 |
| §8.1、§8.2 | #341.5 | #344.2（なし）、#371.3、#378.5、#390.3 |

- 反映した置換は次のとおりです。
  - #344.2：§6.1 の「既定 flag」の文言、Chromium の golden 比較の方針（`question-cd844616…`）、
    `request_timeout` の「（30秒）」。
  - #371.3：§6 の LSP の行（`--die-with-parent` を残す）、§8.1 の exec／death の行
    （「第2.5節の拡張」から「実測5の拡張」）。
  - #378.5：§8.1 の末尾、§8.2 の直前に置く strace の段落。
  - #390.3：§6 の表に js-worker の行を追加。旧 §6.2 の前に新しい §6.2（js-worker の例外）を
    挿入し、旧 §6.2 は §6.3 になった。§8.1 の delivery の行と A の実 tail の行の文言。
- 反映済みの置換のうち、§6 と §8 に当たる分だけを適用しました。#350.2、#353.4、#395.0 は、
  §6 と §8 のどちらも変更していません（確認した）。
- #348.0 の `read` の結果（#344.2 の後の原本の 5 行分の表）と、ここに書いた §6 の先頭 5 行が
  一致することを照合しました。それ以外の照合手段はありません。

## 復元できなかった部分、復元しなかった部分

- **原本そのもの：見つからなかった。** 下記の再構成は、履歴の追記と置換を適用した結果であって、
  原本との byte 単位の一致を確認していません。SHA256 は上記の値と一致するか確かめられません。
- **§0〜§5（対応表、§1 kernel、§2 実測、§3 guard の state machine、§4 transport、
  §5 helper の発見・配布）：復元しなかった。** 要求の対象が §6.1 と §8.2 だったためです。
  履歴の追記（#308.16、#311.2、#314.3 など）には書かれていますが、このファイルには
  含めていません。
- **§7（3 PR の計画と scope）、§8.3、§9：復元しなかった。** 同じ理由です。
  #319.4（§7）と #341.5（§8.3、§9）に最初の追記があり、後の置換が #350.2、#353.4、#371.3、
  #390.3 などにあります。必要なら同じ手順で復元できます。
- **scope の表（`d3a2-B-probe/scope-proposal-v3.tsv`）：見つからなかった。**
  $TMPDIR から消えています。#350.2、#395.0 のコードは生成手順を示しますが、
  結果の表そのものは履歴の出力に一部しか残っていません。復元していません。
- 履歴から読めなかった節は、ありません。ただし、**置換の見落とし**が無いことは、
  `d3a2-B-design-v3.md` を書き換えた `run_js` の結果行（#309、#312、#315、#320、#342、#345、
  #351、#354、#372、#379、#391、#396）を、`thread_grep` で全て確認した範囲でのみ言えます。

---

## 6. 全launch経路と挙動の変更点（MB1）

| 経路／owner | 現在の入口 | Bの適用／完了PR |
|---|---|---|
| shell pipe / ShellJob | tools/shell/jobs/process.rs:18–43 | WrappedCommand→typed GuardedCommand→GroupChild、B2 |
| shell PTY / ShellJob | 同:100–153 portable_pty/register_group | helper argv前置き＋guard-backed Child/Killer、B3 |
| **js-worker / SandboxSession** | **tools/run_js/worker.rs:438–450、evorch/src/main.rs:7** | core側StdioSession/bwrapだけをguard、B2。workerは子processを起動せずcoreへtool RPCするのでInstalled bind対象外。minimal sandboxへhelper/bundleをmountしない。境界試験をB2、実残存セルをB3へ |
| session / SandboxSession | sandbox/session.rs:59–73 | sync guard＋stdio/try_wait/output/Drop、B2 |
| ledger / 全8kind | ledger.rs:247–258 | fence意味維持、保護は登録前。optional ledgerなしも必須、B2（PTYはB3） |
| LSP | lsp/client.rs:32 spawn_blocking→StdioSession | bwrap/directとも外側guard、既存--die-with-parentは残す、thread退役で死なない、B2 |
| MCP stdio / McpStdio | mcp/transport/stdio.rs:590–619 | stdoutにhelper protocol混在なし、B2 |
| renderer / Renderer | workbench-host/areas/browser/renderer.rs:513–548 | rendererをguard、CEF native子は同groupまで、B2 |
| agent Chromium / Other（新enum不要） | **browser/agent/chromium.rs:249 Browser::launch**→chromiumoxide内部spawn、tools/browser.rsから使用 | **browserが所有GuardedCommandで起動、Browser::connect_with_config**。ledgerなくても保護、B3 |
| GUI external slash | **gui/model/slash_process.rs:33**、external_slash.rs | 同じtyped builderへ移行。GUI自身のpidfdを監視しGUI終了でcleanup、B2。coreの子と誤称しない |
| PR source / PullRequestSource | runtime/pull_requests/source.rs:217–258 | Some/None ledger両分岐guard、B2 |
| eval groups / Eval | eval/evaluator/groups.rs:93–132 | 旧PDEATHSIG(TERM)削除、member.pidはtarget_id、B2 |
| eval verify | eval/verify.rs:75–104 | GroupChild移行、exited＋drain→kill_and_waitの2段を維持、B2 |
| Probe ChildKind | 定義済、production callsite発見なし | enumを除外せず8kind fixture必須、B2/B3結合 |
| **core-launcher private preflight** | **core-launcher/handoff/supervisor.rs** | 既存fork-only supervisorのEOF/PDEATHSIG/未reap anchor→groupkill→reapを維持。新guardへ重複登録しない。core死/親thread退役/失敗時のprivate probe cleanupを既存handoff試験で再検証、B3 |
| Other | grep/git-diff/post-edit、runtime git_read/snapshot、providers codex quota、sandbox bwrap/minimal detect等 | 共通GuardedCommand/GroupChild/StdioSession、B2 |

GroupChild.id/StdioSession.idは実際のcore child＝guardのPID。公開eval PIDとrenderer test hookはtarget_id()へ移す。coreからtarget_idをPGID killに使わない。grep監査に加え、**外部crateが内部spawnするpathも別途監査する**（型拒否だけではchromiumoxideに届かない）。新経路発見時は表/scopeを更新し親確認。

### 6.1 agent Chromiumの具体方式

親question-7cb655...の承認案。chromiumoxide0.9.1のasync_process.rs:23はkill_on_drop(true)固定、Browser::launch初期化失敗時もchild.kill()を直接呼ぶ。helperをexeに指定するだけではguardが先に死ぬので**採用しない**。vendor/外部crate patchも入れない。

- browser側が既存configから **実際に渡るeffective argv/env/cwd/user-data-dir** を自前LaunchSpecへ組み立て、GuardedCommandでChromiumを所有起動する。childはRunningが所有。detect（CHROME/executable）の意味、proxy、profile、headless、window、既定flag集合/disable-featuresのmerge順を変えない。
- chromiumoxide既定argsはprivateなので暗黙再利用せず、**0.9.1の現在のeffective argvをgoldenで固定したlocal adapter**を作る。旧BrowserConfig::launchをargv/env capture fixtureへ向けた契約試験と、新adapterの結果を比較する。upstream argument.rs:4/31のArgsBuilderはHashMapで、別keyの順だけ元々非決定。親question-cd844616f94b48c79e3547eb7e66fa09により**key間順だけ正規化**し、全switch/value、同key内merge順・重複、env/cwd/profile/HandlerConfigは完全一致させる。envも全key/valueを比較し、無関係なenvp順だけ集合として扱う。依存更新で変わったらgolden差をレビュー。一部flagだけの比較、同key値のsort/dedupで差を隠すことは禁止。
- HandlerConfigを明示し **ignore_https_errors=false、ignore_invalid_messages=true、viewport=None、context_ids=[]、request_timeout=既存super::REQUEST_TIMEOUT（30秒）、request_intercept=false、cache_enabled=true** を旧launch結果と同じにする。Browser::connect_with_configに、同じ単一Browser object/Handlerを持たせ、既存gate/connection/Shared/finished/retryの構造と世代管理を保つ。
- stderrからDevTools endpointを読む。**ExecOk後から既存20秒、総受信256KiB、1行16KiB**。UTF-8/EOF/error/期限/上限でLaunchError。bounded tailを診断に使いsecret redactする。旧判定と同じlistening on→ws...devtools/browser形式に加えloopback endpointであることを確認する。成功後は旧parser同様stderr readerをcloseし、無関係なloggerや新無制限drain taskを足さない。
- endpoint/接続/initial CDP設定の失敗は、所有childへForceKill→root/helper reap。fork後でcancel/dropされてもDropが同じcleanup。chromiumoxideのchild-less Browser::wait/killをOS cleanupとして呼んで済ませない。
- 明示closeは**同じBrowserへCDP Close→owned child.wait**、CDP失敗/既存caller timeoutならForceKill/reap。Dropは旧kill_on_dropの役割をowned wrapperが引き受けgroupごとcleanup。handler/gate/proxy/profileのrelease順も試験する。新しいgrace timeoutは足さない。
- O-V1/O-V2 screencast、#714のretry（同じBrowserとgenerationを前提）を壊さない。実browser-e2e、agent_observation_chromium、再接続/retry、実Chrome下のcore74をB3受入に入れる。Chromiumのsetsid/setpgid traceも採り、別groupがあれば正確に境界を報告する。全Chrome子孫が同groupだという未検証前提を置かない。

20秒/256KiB/16KiBはendpointの読取上限で、Aの期限やchild kill graceではない。新readerが大量stderrで失敗する変化は明示し、上限直前/直後/改行なし/不正UTF-8/endpoint直前exitを試験する。

> 注（この復元文書の読み手へ）：上の「明示close」は、承認 review が指摘したとおり、現在のコードに
> browser 全体の明示 close が無いため**新設になる**。承認 review は B3 で足さないよう求め、
> DB-21 に「browser 全体の明示 close を新設しない」と入れた。ここは設計 v3 の原文のままで、
> 注は復元者が足した。

### 6.2 js-workerの例外を保つ境界

親question-3aef46...の条件。js-workerはminimal sandbox内で自身の子をspawnしないのでhelper発見をしない。許可するtool呼出しはHostCalls/RPCのみ。将来この前提を変えるとguard配布まで設計を見直す必要があり、暗黙の例外拡張は禁止。

- B2のevorch/tests/js_worker_boundary.rsでsyn（既存workspace依存）によりproduction moduleを解析し、**import aliasを展開した外部path allowlist**を固定する。std::processはExitCodeだけ、tokioのspawn_localはtaskで許すがprocess APIは許さない。Command/async_process/portable_pty/sandbox spawn/libc fork・exec・posix_spawn、unsafe externや未知moduleへの委譲を拒否。js-engineとtools/run_js/protocolのproduction module closureも検査し、外部crate更新はallowlist/review対象。単なる文字列grepでCommand aliasを見逃さない。sourceはCARGO_MANIFEST_DIRをruntimeに取得して読み、include_str!/env!で他packageをbuild keyへ混ぜない。
- 別名import、fully qualified呼出し、helper関数/module経由、extern/unsafeへのmutationでboundary失敗、単なるtokio task spawnで誤検知しないfixtureを入れる。追加許可が必要ならこの前提のレビューを先に行う。
- B3のrun_js_e2e/core matrixに **本物evorch js-worker** を追加。READY出力後 **await new Promise(() => {})**（loopなし、60秒timeout）で待機し、core74後のworker＋同group終了eventを確認する。10セルを消さず**11root**に増やす。real MinimalSandbox/bwrap integrationと、既存WorkerLauncher::injected DirectSandboxのfixtureを両方使う。後者ならbwrapの既存--die-with-parentがguard欠落を隠さず、guardを外すmutationでworkerが残り失敗する。workerのHostCallsだけをpendingにするとcore EOFでpending.clearされ自然exitし得るので、その偽陽性fixtureは使わない。
- 実workerのclone/fork/exec traceを外側fixtureから追い、初期exec後の別process生成0（CLONE_THREADは別扱い）も確認。boundary試験と実protocol試験を組み合わせ、guard境界をworker内へ広げない。

### 6.3 evalとPTY

**evalの変化**: 以前はcreator threadが終わるだけでPDEATHSIG(TERM)、core死もTERMだった。B2ではthread退役は無通知で生存、core process死はguardにより **SIGKILL**。TERM handlerのflush等はcore死では走らなくなる。これはD-7強制停止契約の意図した変更。通常Stopは従来TERM/grace→必要時KILLを維持。TERM handlerのACKを使う通常Stop回帰と、core死時にACKせずsignal9となるセルを分ける。

**PTYの境界**: 製品shell jobはtools/shell.rs:440,523の **sh -c** で通常job control無効。一般のcommandは初期shell groupに残り対象内。実測interactive bashは積極的にjob controlを有効にした境界例で、製品PTYの全commandが別groupという意味ではない。

portable_ptyがhelperをsession leader/ctty ownerとして起動。helper fork先shellは別PGID leader、tcsetpgrpを済ませてからexecを解放。helperとshellは同SID・別PGID、SIGTTOU等のsetup mask/dispositionをtargetに漏らさない。resizeは従来master ioctl。raw Child/KillerでguardだけをkillせずIPCへ。shellの初期PGID外へ出るinteractive job/setsid等は保証外、root自体はPDEATHSIGの補助対象。

---

## 8. 試験計画（全て未実装／未実行、完了時の必須証拠）

### 8.1 event、所有権、mutation

固定sleep後のkill(pid,0)だけを合否にしない。READY/ExecOk/RootExited/TAIL/TERM_ACK/FinalStatus、pidfd readable、controllerのwait結果で同期する。time limitはhangの失敗上限であり成功根拠ではない。外側test controllerだけをfixture subreaperにして、**fixtureが起動した全processのpidfdと終了通知**を保持し、finallyでSIGKILL/reap→ECHILD。core/guard製品にはsubreaperを追加しない。失敗mutationや保証外groupもpidfdで正確に回収し、他processのnumeric PGIDをkillしない。timeout時scope全体の回収後も各fixtureのwait結果を記録する。

| セル／PR | passのevent・観測 | 外したとき失敗するmutation |
|---|---|---|
| delivery/B1 | 3Nix packageの実ELFからhelper pin→Hello一致。CLI eval、cold/planned core、GUI各entry（js-workerは下の独立セル） | helperを1配布から抜く、wrapper実ELF名を誤る、古ABI helperへ差替え |
| library/CI全group/B1 | 各job/shardがproduct helperを受け取り、ledgerなしの最小spawn成功、Installed env override無視 | runtime groupだけbuildして他groupに配らない、PATH/debug代替を許す |
| artifact/F4-P/B1 | schema2/hash/mode/recipe/outside-target/4file copyを満たす。core＋helperどちらの1byte破損も拒否 | schema1やhelper nullをLinuxで許す、coreだけseal/copy |
| pin/B1 | hash後にpathnameをatomic replaceしても**pin済みold inode**が起動、新規bindは新hashで拒否。argv nonce既知のwrong PID/UIDは拒否 | pathname再open、nonceだけ認証 |
| handshake/B1 | 部分frame/slow peerで5秒絶対期限、Ready前にはtarget READYが絶対来ない。Ready後Commitなしもtargetなし | chunk受信でdeadline更新、認証/Ready前fork、wrongpeer無制限 |
| core death起動中/B1 | auth前、Ready前、Commit後pre-exec、ExecOk直後の各barrierでcoreをSIGKILL。target存在時はpidfd exit | getppid照合またはcore pidfd pollを外す、exec error pipeだけblocking wait |
| leader reservation/B1 | root exit37をWNOWAITで保持、kill→reap→FinalStatus37。root自然exitとForceKill競合でもdouble waitなし | reap→groupkillの順序反転をtrace invariantで検出。有限PID churnだけで安全証明しない |
| raw status/B1/B2 | exit0/37、SIGTERM/SIGKILL/ABRT（core dump bitを含む）、exec ENOENT/EACCES/ENOEXEC、cwd不在を既存std結果と比較 | helper exit0をtarget statusに返す、signalを128+Nへ変換する |
| signal/終了待ち/B2 | TERM_ACK後rootがgate待ち中はguardが生存。Stop→必要時既存KILLで終了。signal配送Ackとroot exitを区別 | TERMでguard自身exit、kill要求だけでwaitを成功にする |
| **verify/MB2/B2** | root37→exited→孫TAIL→drain→wait37。別セルはpipe保持timeoutから既存1秒reapへ | **RootExitedで自動groupkill**、wait開始時rootまでkill、子孫exitをFinalStatus条件にする |
| session/B2 | try_wait Running=None、root exitだけでSome、stdio reader/limit、Drop、cancel→再wait同一status | root未exitでtry_wait blocking wait、status decoderをfuture内に置きcancelで紛失 |
| FD/stdio/B2 | target fd0/1/2だけ、helperに0/1/2なし。stdin EOF/大量stdout/stderr/読手Dropを本物pipeで同期 | helperがstdio writerを保持、control fd leak、close_range失敗時direct fallback |
| loader env/B1/B2 | sentinel LD_PRELOAD/LD_LIBRARY_PATHでhelperに注入されず、targetだけに適用。env_remove/clear/PATH/nonUTF8/cwdを比較 | helperにもtarget envを渡す、target envでbundle探索 |
| creator thread/B2 | 実bwrap/directを短命pthreadとspawn_blockingから起動、thread終了後もREADY/要求応答、core死後exit | targetの親をworker threadのままにする、eval旧pre_exec TERMを残す |
| 8kind＋ledgerなし/B2/B3 | 所有先と登録前barrier各1、満了後各rootと同PGID孫のexit。eval公開PID=target_id | あるkindをguard対象外、spawn return後だけ登録、evalへguard PIDを公開 |
| **Aの実tail結合/B3** | 実release coreでadmission ticketを保持して最初のfenceを止める。TERM→本物10秒→core74→各child終了event。A34cell不変 | **B guardを直spawnへ戻すと測定6同様fixtureが残る**。API matrixは従来10＋実js-worker1の11root。実bwrapのPDEATHSIGだけで消えるセルをguard mutationの証明に使わない。A timeout値短縮やtail追加で代用しない |
| PTY/B3 | 製品sh -c root/同PGID孫、tty fgPGID=target、resize、Ctrl-C、stdin EOF、raw killer Drop、失敗setup cleanup | helperだけkill、fgPGID=guard、helper master/slave fd保持 |
| PTY境界/B3 | interactive jobの別PGID/setsid孫は保証外として観測・記載、最後はcontrollerがpidfd回収 | scope外の生存を隠す／製品sh -cの同group子まで除外する |
| **Chromium/B3** | argv/env/config golden、endpoint上限/20秒/失敗/drop/close、実Chrome core74でroot＋同group exit、O-V1/O-V2/#714実browser-e2e | Browser::launchに戻す、connectだけにしてownedchildなし、raw kill_on_drop(true)、helperをexe前置き |
| renderer/CEF/B3 | 実renderer Hello/CEF初期化＋navigation/screencast→core74、renderer/同group CEF終了event | ChildKind fixtureだけをreal CEF試験の代わりにする |
| preflight/B3 | actual handoff probeのguard-only配置、core死/EOF/TERM/exec失敗でprobeと同group終了、旧handoffセル維持 | preflightを経路監査から省き、probe cleanupを外す |
| exec/death/B1/B3 | main以外のthreadからexec→同TGID pidfdは非readable、最終exitでreadable（実測5の拡張） | thread fdまたはold tidを監視する。実効下限5.11環境でも確認 |
| GuardLost/B1/B2 | guard先行死はroot exitを観測してもGuardLost/unconfirmed。残る孫はcontroller cleanup | clean successへ丸める、core側PGID fallbackを足す |

強制故障を製品の環境変数/設定hookにしない。standalone helper fixture/外側controller/テスト専用実装差替え、既存の公開boundaryだけを使う。mutationごとにfixture start証拠と失敗assertを保存し、unit test名をgrepして済ませない。guard control buffer圧迫/部分write中のhalf-closeも試験し、ForceKillのsticky性を確かめる。real Chrome/CEFの別groupは記録し、scope外だからと測定後残さない。

順序/retireセルはunit state machineに加え、実core/guardを含む外側fixture全体をstrace -fで追い、対象PIDのreap前のgroup signal、reap後signalなしを確認する。CI runtime/supportへstraceを明示installし、有限16forkのPID非再利用だけでmutationを通さない。

### 8.2 完成品性能gate（親question-7acf169...承認、事前固定）

第2節の小型試作40回やfork CoW測定を完成品の性能合格には使わない。**B3完成後、real loaded release core**のA基準build（B着手直前base。Aを含む、Bを含まない）とB候補buildを比較する。両方のsource/tree/build ID、flags/features/toolchainを凍結し、core以外の無関係変更を混ぜない。Linuxのdirect fallbackや製品fault hookを性能測定のために追加しない。

実行前にconditions.jsonへ以下を保存してからsample収集する。

- uname/kernel、CPU model/count、governor、memory、system負荷、rustc/cargo、release profile、RUSTFLAGS、libc、helper/core SHA、feature-free coreのbuild recipe。helperは本配布manifestのrelease品。CargoはGC=false/直列、**buildは測定scopeより先に完了**。
- 各roundをreproduce-under-limits.sh **--cpus 2 --memory 2G --tasks 512 --timeout 600s --stress-cpus 1** 内で行う。stress-ngがあることを先に確認し、なければabort（scriptのbusy-loop fallbackは使わない）。負荷をscope外に作らず、host実行は改めて承認を得る。
- A/B各実coreを別fresh DB＋同一公開設定で起動。各DBは20既存thread×50message×8KiB、repositoryは1,000 file×16KiB、2 active agentがmock provider fixtureを通じ100ms timerごとに短いread-only tool turnを進める。preparationもbounded、両core LOAD_READY後に測定。APIで作った履歴を使いDB内部schemaや専用製品heap hookは変えない。RssAnon/PSS/thread/child数を記録し、試作の256MiB人工heap量と同一だとは称しない。
- public shell toolで実sh -cを使い、**grep -F -- D-7 fixture.txt / git diff --quiet -- tracked.txt / :** の3workload。fixture.txt=1MiB、needleあり・stdoutはbounded、git対象はclean。最後は製品sh -c ':'に相当。各tool request送信→完了responseの同一controller CLOCK_MONOTONIC RTTを採る（IPC/出力処理も含む）。A/Bのfixture、env、cwd、実sh/grep/git version/pathを一致させる。
- workloadごと各arm20 warm-up、その後 **200 paired sample×3round**。A/B orderは決定的ABBA交互（seedと順序列を保存）、各round独立fresh core。全成功sampleを使い、遅いoutlierは捨てない。OOM/timeout/agent errorがあればそのround全体invalidとして原因を報告し、条件変更して都合のよいsampleだけ足さない。再測定は条件変更を明記し再レビュー。

**採否**（nearest-rank quantile、各workload/各round独立）：B−A **p50≤2ms、p95≤5ms**。ForceKill要求送信→root exit eventの **p99<1秒**（既存reap budget、子孫drainではない）を各round200回以上。100guard保持後180秒のPSS **median≤4MiB、p95≤8MiB**（guard分布、core/targetは別記）。memoryセルも同じ限界内、180秒までtimer sleepして/proc/smaps_rollupを読む。100guardはsleep/pauseするtargetを持ち、active loopを作らない。

超過／測定不能なら最適化・原因切分け後に再レビューし、合格扱いしない。基準を事後に広げたりgroup安全性を弱めたりしない。B3ローカル受入で証拠raw CSV/conditions/summary/cleanupを残す。CIはevent/機能試験を実行するが、hostのsystemd/hardware/load再現性がないrunnerでこのspeed数値をflaky assertionにしない。最小kernel5.11、load scope、実CEF/Chrome availabilityなどCIで確認できない項目は未確認として列挙する。
