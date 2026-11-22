# B3-08 real Chromium and real CEF under a real core, SIGKILL (Linux)

Separate harness=false integration leaf. It reuses the B3-06 kill observer
(`shared-test-support/kill_observer`) read-only through `#[path]`; the B3-06
matrix (`tests/core_sigkill/`) is unchanged.

## What is claimed, and what is not

P* = the sealed feature-free release `evorch-core` closure
(`scripts/product-artifact.sh`) plus external dependencies and provider
configuration. Both cells are **P\***: the product ELF is started through its
own `--foreground` entrance and driven only through the public protocol and a
scripted model provider. There is no bootstrap feature and no administrative
hook. T cells (B3-06 `chromium_t`/`renderer_t`, which launch an external
fixture through the owner API with `core_owner:false`) are not replaced and
not repeated here.

| Cell | Evidence | Entrance | Real engine |
|---|---|---|---|
| `chromium_p` | P* | Conversation `CreateExecution` → agent tool `browser_navigate` (the product's model review of the URL is answered by the mock) | installed Chromium/Chrome (`CHROME`), launched by the core's owned launch (B3-04) |
| `chromium_stopped_p` | P* | same as `chromium_p`, plus SIGSTOP of the non-root, non-guard members before the SIGKILL | same |
| `cef_p` | P* | Conversation turns → `render_artifact` → `inspect_artifact` open, snapshot | `evorch-renderer` (`EVORCH_RENDERER_EXE`, product `EVORCH_RENDERER_EXEC` override) with `evorch-cef-helper` and the pinned CEF |
| `cef_stopped_p` | P* | same as `cef_p`, plus the same SIGSTOP | same |
| `chromium_navigating_p` | P* | same entrance as `chromium_p`; the fixture accepts the request and never answers, so the core is killed **while the navigation is in flight** (event: the fixture read the request line) | same |
| `cef_navigating_p` | P* | same entrance as `cef_p`; the page's script allocates and touches 384 MiB, then never ends, so the core is killed while `inspect_artifact open` is in flight. Event: a member of the guard subtree other than the target root and the guard has a resident set of at least 300 MiB, which only the page's script produces (`cef_p` asserts every such member stays below the threshold when no script runs). Starting the renderer or sending Hello does not satisfy it | same |
| `renderer_launch_failure_p` | P* | same entrance as `cef_p`; `EVORCH_RENDERER_EXEC` (the product's own override) points at an external script that exits at once. Fault injection: **external executable swap** | none (the replacement exits) |
| `cef_init_failure_p` | P* | same entrance as `cef_p`; the real renderer runs with a `libcef.so` that is only a symlink in an otherwise empty directory placed first in `LD_LIBRARY_PATH`, so CEF finds no resources beside the library. Fault injection: **environment** | real renderer, CEF initialization fails (the product reports `renderer IPC failed: PeerClosed`) |
| `classify_control_t` | T (harness control, not the product) | a live process whose parent changed must be classified alive, never dead; a dead one dead | none |
| `harness_timeout_control_t`, `harness_panic_control_t` | T (harness control) | the real `chromium_p` flow in a child controller with an injected whole-cell timeout / panic while the provider's response is held at its gate; the controller must finish in bounded time and every pinned process must be dead | real Chromium (inner) |

Chromium and CEF are separate paths; neither stands in for the other, and the
renderer cell disables the agent browser tools, the Chromium cell never opens
an artifact.

Not claimed: the actual 10 s expiry / exit 74 after navigation (follow-up PR,
after B3-07), a product ELF expiry, performance, `setsid`-escaped descendants
as a guarantee, or that these cells prove the guarantee for any other caller.

## What each cell observes

1. The fixture side of the navigation, as an event: for Chromium the cell's
   own HTTPS server received `GET /b3-08` **and** the tool result carries the
   page's heading; for CEF the `inspect_artifact` snapshot (a CEF DOM read)
   carries the heading. A provider response held at a gate arrives only after
   that tool result reached the provider, so "navigation complete" is that
   arrival, not elapsed time.
2. The owner-rooted process tree (pidfd-pinned, `/proc`-revalidated): the
   target root, its guard (`--launch`, owner argv equals the launched core),
   every descendant under the guard, with PID/PPID/PGID/SID/starttime, process
   type and whether it shares the root's group (`same_group_as_target_root`).
3. Only the core is sent SIGKILL (through its pidfd). Every pinned guard
   subtree member must report death by pidfd **before any cleanup**. Death is
   decided by the pidfd alone: a member whose parent, group or session changed
   is still alive (it is reported as `relation_changed_but_alive`) and is
   still required to die. Members that are already dead before the kill are
   reported as `exited_before_kill`.

### Negative cases (finite time, nothing left behind)

`renderer_launch_failure_p` and `cef_init_failure_p` kill the core after the
product reported the renderer failure, and `*_navigating_p` kill it with the
navigation in flight. Each ends in bounded time, requires the core to die by
SIGKILL, and requires every process outside the owner-rooted graph that carries
the cell's private directory (observed before the kill) to die. In the two failure cells the renderer has already
exited when the core is killed, so there is **no live descendant to observe**:
they establish that a failed start leaves nothing behind and that the cell
terminates, not that a guard kills anything. The launch-failure cell also
requires that the replacement executable ran (it wrote a marker file); for the
init-failure cell the check that CEF, not linking, failed is only indirect:
the same environment without the symlink trick is `cef_p`, which passes.

### Harness recovery (T)

The provider's held response is released by a drop guard before the mock is
dropped, so a whole-cell timeout or a panic cannot block on the mock's join.
`harness_timeout_control_t` and `harness_panic_control_t` test this: with the
release removed the controller hangs and the control fails after its own
70 s bound.

Evidence JSON (`ready`, `death`) goes to `EVORCH_B3_EVIDENCE_DIR`.

## Chromium trust and HOME

`browser_navigate` upgrades http to https and the product proxy refuses
non-TLS, so a successful navigation needs a certificate Chromium trusts.
Chromium 157 does not read `SSL_CERT_FILE`; the only way is an NSS database
at `$HOME/.pki/nssdb`. The cell builds that database with `certutil` inside
**its own temporary HOME** (refused anywhere else) and gives the core only
that HOME; the real `~/.pki` is never read or written. The cell checks that
the product's Chromium process runs with exactly that HOME. A missing
`certutil` fails the cell (CI installs `libnss3-tools`; locally set
`EVORCH_B3_CERTUTIL`). A missing `CHROME`, renderer, helper or CEF also fails;
nothing skips.

## Crashpad and other processes outside the guard's group

Observed with Chromium 157: the browser starts **two `chrome_crashpad_handler`
processes**, each in its own session and process group, already re-parented
away from the browser when the tree is captured. They are not reachable from
the core by parentage and are not in the guarded group, so ownership of them is
**not** guaranteed by the guard. The cell finds them independently (any live
process whose command line carries the cell's private directory and is not in
the owner-rooted graph), pins them by pidfd, records them as
`unreachable_marked` with their group/session, and requires their death after
the core SIGKILL too. They did exit, because they follow the browser's death,
not because the guard owns them; this is not an ownership claim. A surviving
crashpad handler (or any such process) fails the cell. CEF started none in the
same observation. Chromium aux processes (zygote, GPU, utility, renderer)
share the target root's group; the guard itself is in the core's group.

Chromium and CEF children also exit by themselves when their browser/host
process dies. So `chromium_p` detects a missing guard (nothing kills the
root), but **neither natural cell can** tell a group kill from a root-only kill
followed by those self-exits. `cef_p` does not even detect a missing guard on
its own: the real renderer exits by itself on IPC EOF (it is detected only
when the guard bypass is combined with a renderer that parks after
`PeerClosed`, see the mutations below). The `_stopped_` variants (`chromium_stopped_p`,
`cef_stopped_p`) close that gap by fault injection: after the tree is pinned,
every guard-subtree member except the target root and the guard is SIGSTOPped
through its pidfd, so it cannot notice the death and exit by itself. Only a
kill that reaches it ends it, and the cell requires that before any cleanup.
Delivery of SIGSTOP is not awaited, so these variants prove "a group member
that cannot cooperate still dies", not a timing property. They are an
injected-fault cell, not the natural lifecycle; the natural cells stay as they
are. Crashpad handlers are not stopped (they are outside the guard).

## Bounds and execution

Failure watchdogs only: every wait is bounded by 30 s (READY, client, the
navigation event, core death, group death, reaps), the whole cell by 120 s, then
30 s of controller cleanup and a 5 s runtime shutdown bound. Success comes from
events and pidfds, never elapsed time. No virtual Tokio time is mixed with the
real process I/O, and no speed threshold is asserted. Cleanup runs after success,
failure and panic with retained pidfds and reaps adopted descendants to ECHILD
in the dedicated subreaper controller.

Build/seal with `scripts/product-artifact.sh` and export its variables. Then:

```sh
export CHROME=/usr/bin/google-chrome EVORCH_B3_CERTUTIL=$(command -v certutil)
export EVORCH_RENDERER_EXE=$PWD/target/debug/evorch-renderer   # + evorch-cef-helper beside it
cargo nextest run -p evorch-core --test core_browser_sigkill --run-ignored only \
  --test-threads 1 -E 'test(=chromium_p)'   # or cef_p
```

The cells are listed only with `--ignored`, so ordinary test groups never run
them. CI runs the Chromium cells and the three T controls in browser-e2e and the CEF cells in cef-e2e (the exact filters are in `.github/workflows/ci.yml`).

Mutations (guard bypass, EOF-resistant renderer, root-only kill, no NSS trust,
no navigation, no OpenArtifact for `cef_navigating_p`, death decided by relationship instead of pidfd, no gate release)
run in a disposable checkout; no mutation code is committed. A guard bypass
(`Lease::force` as a no-op) alone fails `chromium_p` but **not** `cef_p`: the
real renderer exits by itself on IPC EOF. `cef_p` fails only when the guard
bypass is combined with a renderer that parks after `PeerClosed`.
