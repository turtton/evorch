# ローカルコアの発見・起動調停（ADR 0045 / P10a）

## 境界

- `core-instance`: config identity、private socket path、endpoint record、ロック。runtime / storage / GUI / protocol に依存しない。
- `core-launcher`: `workbench-client` のhandshakeとプロセス起動を合成する。GUI / CLIにはまだ組み込まない。
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
- **18b**: config単位の永続restart budgetを起動決定箇所へ追加する。現在は単一cold-start試行だけでクラッシュ再試行をしない。AlreadyRunningは予算消費対象にしない（`LaunchStage::AlreadyRunning`）。単なるpollingや接続を試行回数として数えない。

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
5. At successor entry, `HandoffEntry::inspect` permits only stdio, lifetime and
   immutable sealed ticket. It consumes/closes the ticket FD. Pass the entry and
   DB-readback expected record to `Instance::adopt_handoff`. Its FnOnce callback
   must validate the complete transaction/manifest and consume the ticket with
   the Adopted CAS, including ACK-loss readback. Only a successful callback yields
   the opaque proof. No launch lock, lifetime reopen/reflock, unlock, or fallback
   acquire occurs. Any failure is fatal. Recreate the signal inbox and process
   pending shutdown **before activation**.
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
weakening inheritance. No target selection/configuration (Q2) is implemented.

Run the actual exec/wrapper fixtures explicitly:

```sh
cargo nextest run -p core-instance -p core-launcher --features core-instance/recovery-test-support,core-launcher/handoff-test-support --no-tests fail
```
