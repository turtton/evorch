# ローカルコアの発見・起動調停（ADR 0045 / P10a）

## 境界

- `core-instance`: config identity、private socket path、endpoint record、ロック。runtime / storage / GUI / protocol に依存しない。
- `core-launcher`: `workbench-client` のhandshakeとプロセス起動を合成する。GUI actor と CLI（`evorch run` ほか）がこれを使う。
- ローカルFS・Linux対象。継承FDのロック所有検証に Linux `/proc/self/fdinfo` を使う。ネットワークFSの排他は保証しない。

## APIと順序

1. `ConfigIdentity::new` でconfigをcanonicalizeする。
2. `CoreBinary::sibling` または `CoreBinary::explicit` で起動対象を検証する。PATH / CWD探索はしない。
3. `Launcher::new(...).connect_or_start().await` が、保存endpointと標準endpointに接続し、config identity・version・必要capability・epoch・Ready状態を検証する。
4. 接続不能・hello前のtransport切断・保存epoch不一致の場合は `.evorch-core.launch` を取得し、recordを最新から読み直して再接続する。identity / version / capability拒否や不正なwire応答は恒久エラーとして返す。launch保持中のみlifetimeをprobeし、切断した旧ownerが後から解放する場合も再probeする。probe FDはspawn前にcloseする。
5. 親はlaunch FDをCLOEXECのまま保持し、fork後の子だけCLOEXECを解除する。子は `--launch-fd` をentryで採用し、CLOEXECを戻す。同じファイルをopen/flockし直さない。
6. 子は `Instance::acquire(&launch, ...)` でlifetime取得後、検証済みstale socketを掃除する。listenerをbindし、handshakeと同じepochのrecordをpublishしてlaunchをcloseする。親も実際のreadiness検証後にcloseする。LOCK_UNやlock file unlinkは禁止。
7. exit 2は再spawnせず既存コアへの再接続へ移る。

endpoint recordはconfig内のatomicな発見ヒントであり生存証明ではない。PIDを記録・参照しない。異なるXDG_RUNTIME_DIRの旧socketもlifetime取得後にのみ掃除する。廃止した `.evorch-core.start` には触れない。

既定30秒のdeadlineは診断だけで、子のkillやlock強奪はしない。起動役が死んでも、spawn前ならlaunch解放後に別クライアントが選出され、spawn後なら子が同一OFDを保持する。子は親のstdout pipeを使わないため親の死によるSIGPIPEにも依存しない。

## 後続PR

- **07**: planned exec / adopt_handoffは別経路。production への配線は 4b。cold launchの採用と混同しない。
- **08 / 17**: GUI / CLIはこのLauncherを使い、embedded runtimeへのfallbackを作らない。client configは再接続時も同じrequest counterを再利用する。
- **18b（実装済み。GUI/CLI への結合は末尾「障害時の再接続」）**: config単位の共有・永続crash予算（`budget.rs`、`.evorch-core.crash-budget`、5分に3回）を起動決定箇所に置いた。数えるのは起動回数ではなく**異常終了の証拠**だけ。(1) lifetime lock が空いているのに endpoint socket が残っている（正常終了は `Instance` の drop で socket を消す。SIGKILL/abort/OOM は消さない）、(2) launcher が起動した core が ready 前に exit 2 以外で終了。正常な起動・終了（idle exit 含む）、予定restart（exec handoff は lifetime lock を保持し launcher の spawn を通らない）、AlreadyRunning合流（exit 2）、polling、接続は数えない。判定・記録は `LaunchGuard` 保持下だけで行い、複数clientが同時に再接続しても二重起動・二重計上しない。窓内3件で次の自動起動を `LaunchError::Budget` で拒否し、再試行も自動リセットもしない（理由文に手動復旧手順を含む）。台帳の破損・不正（所有者/mode/hardlink/symlink/version）も同様に拒否し、台帳は変更しない。記録→証拠socket削除の順なので、間で死んでも過大計上側にしか倒れない。時計逆行で未来になった記録は現在時刻へ補正して永続化する。version mismatch で稼働中coreを停止する経路はlauncherに存在しない。

## 検証

`harness/main.rs` はstorageなしの実プロセスハーネス。control socketのackを各同期点とし、kill後はpidfdの終端イベントで待つ。テスト内のsleepやwall-clock完了判定はない。ハング診断は仮想時間で検証する。実evorch-coreにも継承launch起動の回帰テストがある。


## Linux exec handoff library (07b-4a)

This is a foundation, not a product restart entrypoint. No SafeRestart capability,
core parser wiring, target configuration, database access or runtime dependency is
added here. The feature-gated executable is a test fixture only.

Integration sequence for 4b:

1. Call `core_instance::handoff::BlockedShutdownSignals::at_entry()` before
   creating **any** threads. All threads must inherit blocked SIGINT/SIGTERM and
   never unblock them. Receive shutdown via its CLOEXEC signalfd inbox (not
   handlers). In the successor inspect inherited FDs **before** opening an inbox,
   database, executor or producer; keep the signals blocked across both execs.
2. Record `CurrentElf` at startup for the reserved current target. For explicit
   candidate paths use `HandoffTarget::probe`; call description `require` with the
   effective handoff/snapshot/schema/protocol/platform requirements. Record the
   final ELF build and compiled catalog in the durable transaction. The wrapper
   fingerprint is separate. Re-probe at the last reversible point before capture.
3. Finish durable capture/readback, hold and child/task cleanup in their owning
   layers. Stop the shutdown inbox reader and acquire the final spawn/FD fence.
   No non-CLOEXEC FD may be created, nor a pre-existing non-CLOEXEC descriptor
   or stdio/lifetime allowlisted FD closed/reused, until exec returns or succeeds.
   Concurrent atomic-CLOEXEC opens are allowed. Every thread's signal mask is checked; do not create new threads
   with a different mask. The library is **not** the application's spawn fence.
4. Reconstruct effective argv (config/web options), excluding old launch/handoff
   FD arguments. `HandoffTarget::exec` revalidates with `CoreBinary::explicit` and
   content/inode fingerprints, then uses raw canonical-path execve. Cwd and env
   are unchanged. A wrapper's PATH additions are intentionally preserved, also
   on repeated restart. Do not infer which old PATH entries are removable.
5. Fix the mode with entry::EntryOptions before normal parsing/bootstrap.
   Incomplete/duplicate handoff options, launch+handoff and describe+handoff are
   fatal, never Cold. Inspect inherited FDs before creating threads. Call
   Instance::adopt_handoff(config, ipc_parent, entry) for **physical adoption
   only**. It returns Instance and opaque proof without opening a database.
   Then start the dedicated boot monitor and perform DB Adopt CAS/readback with
   the existing writer inside restricted bootstrap. Keep the same proof on ACK
   loss; do not claim/scan/activate before confirmed Adopted. No launch lock,
   lifetime reopen/reflock, unlock, or ordinary-shutdown fallback is permitted.
   Deliver pending signals to the boot guard before Ready/activation.
6. Any returned exec/preparation error means FailedClosed: retain Instance and
   its lifetime lock, undo owner holds through the owning layer, never retry or
   reopen producers automatically. CLOEXEC, signal mask and dispositions are
   restored. A successor that hangs while holding lifetime is an operator-shutdown
   diagnostic, not permission for a launcher to steal the lock or spawn a core.

Probe limits are 1..=65536 output bytes and (0,60s] (default 16384 bytes/5s).
Both stdout and stderr count. The dedicated syscall-only subreaper supervisor
has its own deadline, parent-death signal and control-EOF cleanup. Timeout,
cancellation, dropped futures and even descendants escaping to other process
groups trigger kill and wait/reap without changing the host's subreaper state.
Leader exit is observed without reaping until group kill, pinning the PID/PGID
against reuse. Signal draining is bounded so the deadline remains observable.
The host must leave SIGCHLD waitable and not externally reap this supervisor;
auto-reap dispositions are rejected before spawning.
As with all SIGKILL/reap designs, cleanup ultimately relies on the kernel allowing
processes to terminate (an uninterruptible kernel task cannot be forcibly reaped).

The target is trusted at the same UID boundary as cold launch, not sandboxed as
hostile code. Probing must enter self-description before DB/locks/MCP/producers.
Symlinks are not enabled. The same-UID path-replacement race between final
validation and exec is not a new trust model; immutable Nix store paths are
recommended. Unsupported /proc/FD guarantees reject preflight rather than
weakening inheritance. Local target configuration and entry helpers are provided
by 4b-A; production coordinator/main/bootstrap wiring remains 4b-D.

Run the actual exec/wrapper fixtures explicitly:

```sh
cargo nextest run -p core-instance -p core-launcher --features core-instance/recovery-test-support,core-launcher/handoff-test-support --no-tests fail
```

## 4b-A integration handoff (not yet a product restart entry)

- At each new request, the coordinator calls
  `Config::load_restart_settings(config_dir)` and `settings.target_path(id)`.
  Pass the resolved path (`None` for startup-recorded `current`) and fixed
  timeout through `TargetSelection`. Launcher intentionally has no config
  dependency. IDs are 1–64 ASCII alphanumeric/`_`/`-`, at most 32 configured
  entries, absolute UTF-8 paths at most 4096 bytes. `current` cannot be overridden.
- `RestartTarget::preflight` precedes quiesce; consuming `before_capture`
  re-probes at the final reversible boundary. `before_exec` never spawns.
  The caller maps failure to cancellation before capture, FailedClosed after it.
  B must retain probe ownership/cleanup; D must finish any re-probe before closing
  the spawn fence. Existing `HandoffTarget::exec` still does heavy preparation;
  4b-D must not place that preparation after owner-worker stop (E18).
- D writes the initial `RestartBootReceipt::restarting` before exec, binds
  `BootDeadline::start()` to the Restarting transition once, and places that same
  deadline in `HandoffRecord`. The sealed ticket binds it across exec. After
  physical adopt, immediately start `BootWatchdog` before blocking bootstrap.
  It holds lifetime even if its handle is dropped. The monitor does no receipt,
  DB, stderr, or destructor work and calls `_exit` at the fixed deadline.
  Pre-monitor physical validation still contains synchronous filesystem/hash
  work: D must resolve/audit that unmonitored window when wiring the entry.
  This fixture does not prove the complete exec-to-Ready 120-second product gate.
- D/ C keep proof + confirmed DB Adopt together. Call `adopted()` after Adopt
  readback, and `shutdown_signal` on pending/new shutdown signals. No children,
  providers, learning dispatch or activation before Ready. After control bind
  and durable Ready readback, `ready()` returns the retained `Arc<Instance>`.
  The API does not itself establish DB Ready; the caller owns that precondition.
- Select failure before diagnostic IO, update receipt and (if available) the
  existing single writer, then `exit_after_diagnostic`. Codes: 70 before Adopt,
  71 definitive rejection, 72 unresolved Adopt ACK, 73 any Adopted-to-Ready
  failure (including recovery preparation, bind and Ready ACK), 130/143 signals.
  ACK loss after Adopted remains 73. No cold fallback, normal shutdown/release,
  hold rollback, or Stop intent. Next explicit cold boot quarantines leftovers.
- The receipt path is config-side `.evorch-core.restart-receipt`, not
  `XDG_RUNTIME_DIR`. It is diagnostic only: 0600, no symlink/hardlink/special
  files, 16 KiB maximum, atomic replace + file/directory fsync. Updates must be
  serialized with diagnostic revisions by D. If both stores fail, D reports a
  bounded stderr diagnostic without extending the deadline; the independent
  monitor remains able to exit even if that reporting blocks.
- 5a uses `diagnostics::read_boot_diagnostic`, distinguishes historical,
  unavailable and unconfirmed-Ready diagnostics, and prefers the authenticated
  live core response. Receipt fields grant no recovery/owner authority.
- Whichever of PR10 and 4b-A merges later must use
  `reject_restart_target_changes` under its
  save revision/serialization guard for full and partial saves. A full omission
  is deletion; partial omission preserves the existing map. The common config
  file-save boundary already rejects target additions/deletions/path edits.
  5a must test the real GUI/Web save routes before publishing restart. Remote
  control access, including Tailscale, must not become arbitrary binary selection.

The approval's A1–A7 are applied here: use **4b-D-1/2/3** for the later PRs,
not constraint IDs D1/D2/D3. A has no migration; C reserves v65, 5b v66, parallel
work v67+. The revised plan overrides the previous version. Production main,
bootstrap staging, full failure-matrix/Ready races and real product-binary e2e
belong to D and 5a, not this package's fixture.

## 障害時の再接続（PR C 第2段階）

- core は正常終了（signal・idle exit・管理 stop）のとき、接続を閉じる前に各 subscription へ `CoreShuttingDown` を送り切る（待ちは2秒で打ち切る）。GUI actor は、その通知も予定 restart の証拠もなく接続が閉じた場合（異常切断）に限り、窓（5分）あたり最大3回まで `connect_or_start` を自動で呼ぶ。起動してよいかは上記の共有予算が決める。正常終了の通知後・予算超過・台帳異常・version不一致のあとは自動では何もせず、理由を表示して人の「再接続」だけを受け付ける。予定restartの後継待ちは connect-only で、予算を消費しない。
- 稼働中coreが version / identity / capability で拒否した場合は `LaunchError::Incompatible`（coreは停止しない）。
- CLI は core を失ったら自分の invocation を失敗（exit 1）で終え、再接続も再送もしない。予算拒否は `CliError::StartRefused`、非互換は `CliError::Incompatible` として区別して表示する。
- 実行中のCLI invocation は idle-exit を止め、status に `cli_invocation` として現れる。
