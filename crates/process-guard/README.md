# process-guard: typed PTY backend (D-3a2 B3-02)

**D-7 is not complete.** This supplies a Linux backend, not the shell caller
migration. Shell PTY owners remain on their old path until B3-03; agent
Chromium, release-core expiry, CEF and the combined performance/ownership
evidence remain separate B3 work. Non-Linux behavior is unchanged.

## Contract for B3-03

```rust,ignore
GuardRuntime::spawn_pty(&LaunchSpec, PtySize)
    -> std::io::Result<(GuardChild, PtyMaster)>
```

- `LaunchSpec` still describes only target argv/env/cwd. `spawn` selects direct;
  `spawn_pty` selects PTY. The authenticated wire request carries the explicit
  mode, which must match the helper's launch mode. No launcher callback, raw
  `Command`, slave/master descriptor, or new numeric PID/PGID API is exposed.
- process-guard performs `openpty`, owns both ends, obtains a CLOEXEC slave via
  Linux `TIOCGPTPEER`, and gives only its three stdio copies to the helper.
  The core retains no slave after spawn, including failures. The returned
  `PtyMaster` owns the master; all cloned readers/writers also keep it open.
  No slave pathname is opened.
- `PtyMaster` is `Send` (not `Sync`) and exposes only `try_clone_reader`,
  one-shot `take_writer`, and `resize(PtySize)`. Readers preserve portable-pty's
  EIO-to-EOF conversion. Writer Drop sends EOT (canonical-mode EOF); it is not
  process retirement. Dropping all master-side handles can cause terminal
  EOF/SIGHUP. Retaining any clone intentionally prevents that close.
- The returned `GuardChild` uses exactly the direct authentication, decoder,
  status storage and reaper. `exited`/`poll_exit` only observe root exit;
  `request_wait` is sticky even if a caller cancels its future. `wait` and
  `try_wait` preserve the root's raw Unix status. `request_signal`/`signal`
  address the reserved target group. `force_kill` half-closes control;
  Drop half-closes and waits for the helper, so B3-03 should perform blocking
  launch/cleanup off async workers. A split/cancelled launch must still drop
  its completed result. Ledger admission/registration belongs to B3-03.
- `FinalStatus` confirms root reap and group-KILL submission, **not** all
  descendants' exit/reap. `GuardLost` is an error, never a successful receipt.
  No bare-PGID fallback is permitted. The guarantee covers the root and
  descendants staying in its original PGID, not `setsid`/other-PGID escapees.

## Session, environment, delivery

After identity verification and PID/UID authentication, but before Ready, the
helper makes itself session leader and acquires the slave as its controlling
terminal. Only Commit permits target fork. Before exec the target creates a
separate PGID in that same SID, suppresses SIGTTOU while installing that PGID
as foreground, then restores default signal dispositions and an empty mask.
A foreground-setting error returns SpawnError without executing the target.
Ctrl-C addresses the foreground target group, not the helper. The helper
blocks/forwards terminal HUP and remains able to confirm the root's status.

portable-pty is the existing workspace version (0.9.0), reused for allocation
and master IO rather than reimplementing them with libc. Its `CommandBuilder`
and `SlavePty::spawn_command` are **not** used: they add `SHELL` even after
`env_clear`. Both backends use the existing private std-process launch with a
completely empty helper environment. Target env/cwd is applied only at target
exec. `TIOCGPTPEER`, session and foreground setup have no fallback.

The approved PTY locator stays `/proc/<core-pid>/fd/N`, retaining the pinned
image through exec/authentication/ExecOk; direct remains `/proc/self/fd/N`.
No distribution pathname is reopened. Shared PID namespace and proc/ptrace
visibility are required; unavailable access fails closed. The locator detects
guard-compatible inode mix-ups, not arbitrary non-guard execution after core
death plus PID/fd reuse. This previously accepted residual PID-reuse boundary
is **not** removed by the new backend.

The absolute Ready deadline remains five seconds, never extended by progress.
Linux >=5.11 remains required. There are no product fault switches, timers,
feature-dependent wire variants, or compatibility migrations.

The package-local normal/build dependency receipt includes portable-pty's
closure. `build.rs` hashes the private backend modules and manifest with the
existing package-only inputs. Changing them requires rebuilding and sealing
a new bundle; old compat/hash bindings are rejected. Moving the already
locked portable-pty edge from dev to normal does not change its existing
`Cargo.lock` dependency entry or version.

## Focused verification

`tests/pty.rs` and its private modules cover native exec tty/SID/PGID/foreground,
signal restoration, extra-FD absence, resize, Ctrl-C, EOF/HUP, sticky/raw status,
exec errno, empty helper environment, pinning, rejected proc access, mode/auth
mismatch, pre-Commit disconnect, post-exec half-close, foreground failure,
same-group descendant cleanup and GuardLost.

The core-death controller retains both the control endpoint and the actual
master via test-only SCM_RIGHTS. Thus neither control EOF nor terminal HUP can
masquerade as owner-pidfd monitoring. Outer isolated subreapers use pidfds and
reap observed descendants even on assertion/mutation failure. Synchronization
uses READY, ExecOk, root exit, FinalStatus and pidfds, with no sleeps or test
completion timeouts; nextest supplies the existing 180-second hang limit.
Existing direct lifecycle/delivery/wire tests cover the shared handshake's
absolute deadline and authentication/compat/loader negatives.

Focused local mutation results (each rebuilt with its own compat/sealed bundle;
all mutations restored):

| Mutation | Detection |
| --- | --- |
| Omit foreground installation | Native target reached READY, foreground PGID assertion failed |
| Reuse direct `setsid` target setup | Native target reached READY, same-SID assertion failed |
| Remove running owner-pidfd monitor | Root and descendant reached READY; after core SIGKILL a live helper ACKed, test failed; outer controller reclaimed all |
| Add `SHELL` after helper `env_clear` | Target reached READY, `/proc/helper/environ` emptiness assertion failed |
| Omit running-image identity verification | With valid UID and a real listener, the wrong-inode helper reached Hello; the isolated authentication negative failed |
| Reopen distribution pathname | Replacement was rejected instead of using the pinned image; separately, denied proc exec incorrectly returned an ExecOk owner, test failed |

These are source mutations in test work only, not shipped fault knobs. The
pathname-replacement failure is an authentication/startup negative after an
already successful original launch, not a claim that its second target ran.
The isolated wrong-image mutation likewise tests rejection before Hello, not
a target-lifecycle guarantee.
