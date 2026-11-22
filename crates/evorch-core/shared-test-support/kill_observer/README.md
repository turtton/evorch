# Shared Linux kill observer — B3-06 / B3-07 / B3-08 interface

Import mod.rs by path from a dedicated integration controller. Test support,
not a product API; independent of caller implementation, core features,
shutdown timeout and provider.

- ProcessTree::enable_controller_subreaper() -> io::Result<()>: call once in
  a dedicated controller, never a concurrent libtest process.
- ProcessTree::new(owner: i32) -> io::Result<ProcessTree>: acquire an alive
  owner's pidfd immediately. B3-07/08 substitute their actual core PID.
- refresh(&mut self) -> io::Result<()>: discover /proc candidates, pin and
  revalidate identity, then recompute owner reachability with **validated
  rows only**. descendants is the current validated snapshot. Obsolete pins
  remain private for cleanup, never authorizing a graph edge.
- refresh_snapshot(&mut self, &BTreeMap<i32, Identity>): identical validation
  with an untrusted injectable snapshot for deterministic discovery races.
- owner: PinnedProcess; descendants: BTreeMap<i32, PinnedProcess>;
  require(pid) -> io::Result<&PinnedProcess> rejects unvalidated targets.
- expect_dead(&self, pids: &[i32], limit: Duration) -> io::Result<()> awaits
  all retained pidfds concurrently under one caller-supplied real-time
  failure watchdog; a failure lists every not-yet-dead target.
- cleanup(&self, limit: Duration) -> io::Result<()> is **not evidence**:
  call after assertions on success/error. Kill owner/known descendants via
  pidfd, discover adopted descendants, reap to ECHILD.
- cleanup_controller(limit) is the final panic/startup-failure fence, even
  before an owner is pinned. Drop is only emergency pidfd kill backstop;
  normal cleanup must be awaited.

Identity is serializable: pid, parent, group, session, starttime and state.
PinnedProcess::{capture(pid), pin(&Identity)} opens pidfd while alive,
re-reads identity and rejects Z/X. signal(Signal) and exited().await use
that fd, **never numeric-PID signalling**. unchanged() revalidates live
identity. Scheduling state itself is not identity. PID reuse/changed
ancestry/failed pins cannot confer authority; cached /proc is not a signal
capability.

bounded(Duration, description, future) is an external failure watchdog, not
a success detector. Supply short real-time limits, never advance Tokio time
with real sockets/processes. EOF/reset may accompany death but cannot
replace pidfd evidence.

Lifecycle: launch → pin owner → event READY → refresh → select targets →
assert identities/PGID/owner → inject only intended event → expect_dead →
record result → cleanup (also errors) → final controller panic fence.
Do not derive success from cleanup or sleep. B3-07 supplies actual expiry;
B3-08 actual browser entrances/navigation, with no owner kind/name hardcoded
in this observer.
