# TaskProgressed maintenance

This change does not run an offline repair at installation or merge. **Do not
run either preview or `--apply` until all evorch GUI/runtime processes have exited.** Use a version
containing this command; do not run older binaries against a store while it is
under maintenance.

## Offline: strip the obsolete cursor, not task history

```sh
# Read-only preview; this is the default, without --apply.
evorch maintenance strip-task-progress-cursor \
  --db "$HOME/.config/evorch/evorch-events.db"

# Stop all evorch processes first. Choose a new, private backup filename.
evorch maintenance strip-task-progress-cursor \
  --db "$HOME/.config/evorch/evorch-events.db" \
  --apply --backup /private/backup/evorch-before-cursor-cleanup.db
```

The preview reports target rows, UTF-8 payload bytes, serialized cursor-value
bytes (including JSON quoting), and estimated payload reduction. It does not
print task text or cursor content. Estimates are not a promise of physical
file-size reduction: indexes, free pages and SQLite packing also matter.

Apply creates a consistent `VACUUM INTO` backup **before rewriting any event**,
checks it, then removes only
`$.payload.payload.progress.resume_cursor` from `Orchestrator/TaskProgressed`.
IDs, envelopes and all other JSON fields remain unchanged. JSON whitespace is
normalized by SQLite's `json_remove`. Ingress SecretGuard/redaction are not
bypassed to insert new content: this operation only removes a field from
already-persisted data. Quota aggregates and the session byte projection are
adjusted in the same transaction.

- Backup defaults to `<db>.before-task-progress-<timestamp>.db`; it never
  overwrites an existing file. Backups contain the old cursor data. Keep them
  private and delete them later under your own retention policy.
- `--no-backup` is an explicit opt-out. `--backup` and `--no-backup` conflict.
- `--batch-size 32` is the default, bounded to **1–256 rows**. Each batch commits.
  Interruption leaves completed batches valid; running again finds only rows
  which still contain the cursor. A failed backup leaves the source payloads
  unchanged; a failed rewrite/vacuum may leave committed progress. Keep the
  original backup and rerun with a different backup path (or explicitly
  `--no-backup`). Never replace the live file with a backup while evorch runs.
- Full `VACUUM` is on by default; `--no-vacuum` skips it. It can take minutes or
  longer, depending on DB size and disk throughput, and is not cancellable via
  an application progress UI. Plan downtime. Output includes before/after
  database sizes and whether vacuum/WAL truncation completed.
- Free-space checks are conservative: allow up to **4× the original database
  size** for backup plus source scratch/WAL. Space checks cannot reserve bytes;
  other processes may consume space afterward. Unix free-space checking is
  supported; other platforms refuse apply rather than guessing.
- Finish with `wal_checkpoint(TRUNCATE)`. A failure is an error, not success.

### In-use detection and limits

New storage writers hold a shared `<canonical-db>.maintenance-lock` lease for
their entire lifetime; maintenance obtains it exclusively without waiting.
An idle new writer is therefore detected. Do not delete this lock file: its
stable inode is part of the protocol. Multiple normal writers remain supported.

Apply also uses `busy_timeout=0`, `locking_mode=EXCLUSIVE` and `BEGIN EXCLUSIVE`,
and keeps the connection/exclusive mode until backup, every batch, vacuum and
checkpoint finish. **Plain `BEGIN EXCLUSIVE` alone is not exclusive of readers
in WAL mode.** Active legacy/non-cooperating SQLite readers/writers cause refusal
rather than being waited out. An older process which has not opened/touched the
DB yet cannot be inferred from SQLite locks. An idle legacy connection without
a held SQLite lock also cannot always be distinguished from a closed process.
This is why stopping *all* processes remains an operator prerequisite. Symlink
aliases resolve to the same lease; hard-link aliases and network filesystems
with unreliable locking are not supported safety guarantees.

Both previews require the existing maintenance lease and a checkpointed,
offline store. They never create even a missing lease: if it is absent, start
current evorch once and exit cleanly first. Canonical paths are used to inspect
the real DB/WAL/SHM, including through symlinks; metadata errors are errors.
A nonempty WAL (for example after a crash) is refused **before SQLite opens
the source**, rather than allowing a read-only SQLite connection to initialize
or create SHM. Start evorch once and exit cleanly to checkpoint it, or use
explicit `--apply` while offline (with the normal backup safeguards).

Only after confirming WAL is absent or zero bytes do previews use immutable
read-only SQLite to avoid all source/sidecar writes. DB/WAL/SHM presence, size
and mtime are checked before and after reading; any change rejects the result.
This is an offline contract, not a supported live snapshot: a legacy process
that ignores the lease can race these checks, and same-size/mtime-preserving
external modifications cannot be detected. Stop all processes, including older
versions, before either preview. No writable-SHM or private-copy fallback is
used.

## Live: replay-equivalent retention

Maintenance inspects at most **one candidate index entry / task per tick**,
using an ID-leading partial index and a fixed upper event-ID cursor for the
pass. Excluded/malformed entries advance the cursor too. A task can be revisited
by later entries; every visit remains bounded. New arrivals beyond the upper
cursor are excluded by the index range, not scanned and filtered afterward;
eligibility and deletion share an immediate writer transaction. Other writers
cannot append between the check and deletion. Normal bounded incremental vacuum
and WAL reclamation follow, yielding to the writer command loop between ticks.

Candidate tasks must be `Completed` or `Cancelled` in the **bounded replay**
of the task's events, without relying on a pre-existing `tasks` table row,
and their latest progress must also be terminal. No progress of any run
generation may follow an earlier terminal fence. `Stopped`, `Failed` and other
states are excluded. Failed is not a terminal fence in the production projection
and can be followed by retry progress; treating it like Completed is unsafe.

For each candidate, retain:

1. The first TaskProgressed with non-null input, if one exists.
2. The latest TaskProgressed (one row if these are the same).

**Delete nothing unless both replays are exactly equal:**

- Full production tasks projection, compared as the complete TaskRecord
  (including input, progress JSON, status, attempts, artifact, failure reason,
  heartbeat, timestamps and attribution).
- Full durable task GUI model, including title, run association, state,
  max-attempt count, last nonempty artifact and detail/failure text. The GUI
  reexports the same implementation that this safety check runs.

TaskCheckpoint and task-related Lifecycle events participate in both replays
but are never deleted. No other event variant is deleted. Run generations are
not combined or reset by a SQL shortcut: both original generation fences run
in replay. If the retained pair would choose a different generation or lose
any state, that task is excluded. In particular, the intermediate-artifact
regression (`input → artifact → terminal-without-artifact`) is excluded.

To bound memory and writer latency, tasks exceeding **256 progress/checkpoint
rows or 16 MiB**, or independently 256 lifecycle rows/16 MiB, are excluded.
Unknown/undecodable records also fail closed. Exclusion favors unchanged
behavior over maximal disk recovery. The rule does not promise every task can
be reduced, nor exact preservation of the deleted historical timeline itself.

Disable live retention in configuration (or use the equivalent environment
layer `EVORCH_DIAGNOSTICS__RETAIN_TERMINAL_TASK_PROGRESS=false`):

```toml
[diagnostics]
retain_terminal_task_progress = false
```

Analyze a checkpointed offline store without applying retention, migrations or
vacuum (the same offline lease/WAL checks above apply):

```sh
evorch maintenance preview-task-progress-retention \
  --db "$HOME/.config/evorch/evorch-events.db"
```

The result reports examined/eligible/excluded task counts and deletion bytes.
The store may grow after it is restarted, so the preview is not a reservation or
exact prediction of a later maintenance pass.

An earlier implementation's all-excluded result was caused by its dependency
on the stored `tasks` projection, which ordinary event append does not update;
it was **not** evidence that exact replay always differs. This rule now derives
terminal state from bounded event replay. Connecting the application's stored
projection to ordinary append is a separate, pre-existing issue and is not
changed here.

The two operations have different roles: retention removes redundant whole
progress events only when replay is unchanged. It deliberately keeps first
input/latest rows, which may still contain old cursors, and excludes oversized
or non-equivalent histories. The offline command removes obsolete cursor
fields from **all** matching progress rows, including those retention keeps or
cannot safely delete. Only the offline full vacuum promises an attempt at
repacking the whole database.

### Read-only observation during development

On 2026-10-26, the then-current live store (schema v24; no v26 index was created)
reported 588 examined tasks, **588 excluded**, 0 eligible and **0 bytes** of live
retention savings under the strict comparison. This is not a promise that the
old history can be compacted: safety takes priority over recovery.

The separate read-only cursor preview reported 9,260 target rows,
893,633,858 payload bytes, 860,780,044 serialized cursor-value bytes and
860,937,464 estimated payload reduction bytes. These counts differ from the
older investigation because the store continued growing. No apply, vacuum or
checkpoint was performed on that store.
