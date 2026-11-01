# Conversation index and reindexing

The GUI explicitly starts the shared storage index owner. Merely opening a
`Storage` does not start indexing. GUI and CLI use the same durable request,
lease, and step APIs; there is no separate CLI indexing engine.

Search keyset-scans the indexed body within visible runs and does not read FTS;
matching happens in the runtime after redaction
([#374](https://github.com/turtton/evorch/issues/374),
[PR #432](https://github.com/turtton/evorch/pull/432)).

If index writes are refused only because the WAL is at its limit while a reader pins it, the request is not settled immediately: the owner probes a non-blocking TRUNCATE checkpoint with bounded backoff for up to 5 minutes per request and resumes automatically; after that the pending targets settle as `capacity_wait_expired` gaps. Other capacity refusals settle as `capacity_paused` as before
([#373](https://github.com/turtton/evorch/issues/373),
[PR #433](https://github.com/turtton/evorch/pull/433)).

## Configuration

In the user `config.toml`:

```toml
[storage.thread_index]
enabled = true
max_bytes = 536870912 # 512 MiB (default)
```

These settings are used when storage opens. The GUI's `reload_config` also
applies the settings through the writer. The new limit takes effect when the
writer processes that command, not when the file is edited. A reload error is
reported to the caller; invalid settings leave the current index settings
unchanged.

Increasing the limit does **not** automatically restore evicted runs.
Reindexing them requires `--include-evicted`, and even that flag never bypasses
the logical budget or physical admission checks. Disabling the index does not
disable direct conversation reads or grep.

## Archived threads

A thread whose archive flag is set (`threads.archived`, mirrored from the GUI
sidebar) is out of the index and out of search, whatever the retention period
or whether deletion is enabled:

- When the flag turns on, the thread's runs become `evicted` with the reason
  `archived` in the same transaction as the mirror write. Their blocks stay
  until the writer's maintenance tick removes them (at most four runs per tick,
  at most 200 rows / 2 MiB per transaction, independent of the index job and of
  `enabled`). A single block above that limit is never deleted by the tick: it
  is registered in `reclaim_oversized` (family `archive-index`) and removed by
  the exclusive startup phase, which deletes it only if the thread is still
  archived. A newly saved or discovered run of an archived thread is registered
  as `evicted(archived)` without blocks.
- Whether a run may enter the index is decided from the thread's archive flag
  now (its run binding or its recorded root run), never from the eviction
  reason: the scheduler (`next_work`) and `reindex-threads` (with or without
  `--include-evicted`) skip such runs whatever state or reason they have.
- Threads archived before this version are handled by the same tick in bounded
  steps: one tick reads and updates at most about 1,000 index rows, however many
  runs a single thread has, and a persisted position (the next thread and the last
  run done inside it) lets it resume after a restart. Nothing is rewritten by the
  migration itself. Search exclusion does not wait for it.
- Every statement of the tick is a keyset range over an index, so its `LIMIT` is
  also its scan bound: archived threads are read in ID order from a partial index,
  a thread's runs in ID order from `(thread_id, run_id)`, and the runs left to
  purge from a partial index that excludes the ones handed to the exclusive phase
  (`purge_deferred`). The tick's cost therefore does not grow with the number of
  unrelated or waiting runs.
- `thread_search`, the host's thread search and the candidate lists leave out
  archived threads, **the caller's own thread included**, and so do the child
  threads in a `thread_read` overview. Reading or grepping an archived thread by
  an explicit ID still works; a deleted (tombstoned) thread does not.
- `reindex-threads --include-evicted` does not revive runs of an archived
  thread, whatever the reason they were evicted for. Unarchive the thread
  instead; only `evicted(archived)` runs return to `pending` then.

## CLI

```sh
evorch reindex-threads [--db <path>] [--project <dir>] \
  [--since <unix-ns>] [--include-evicted] [--wait]
```

Without `--db`, the database is `<user-config-dir>/evorch-events.db`, the same
store used by the GUI. The database must already exist. This is a write command:
it can migrate the selected store and persists requests. Use a backup/copy when
validating behavior, not the original store.

`--project` resolves an existing directory to its canonical path and matches the
stored canonical project exactly. `--since` is a non-negative Unix nanosecond
timestamp (not a date string) and includes sources updated **at or after** that
timestamp. The request freezes its target revisions at submission; later runs
are not appended to it. `--include-evicted` explicitly includes evicted sources.

The command prints one JSON result:

| Status | Meaning |
| --- | --- |
| `accepted` | Another GUI/CLI owner holds a live lease. The request is persisted and delegated; it is **not completed**. Includes `request_id`. |
| `completed` | All fixed targets succeeded, including an empty target set. |
| `completed_with_gaps` | All targets settled, but some have reasoned gaps. The summary includes success counts, deleted-before-submission counts, and gap reasons/counts. Even an all-gap request is not `failed`. |
| `failed` | Invalid arguments, an unavailable database, or a control/persistence failure. An ID is present only after acceptance. |

If a source changes after submission, its frozen target can settle as a
`superseded` gap. In particular, deriving proven provenance for an older source
can advance its revision during the first request. The command reports that
gap honestly and does not automatically rewrite targets or submit another
request. Run a separate reindex request to process the latest revision.

`accepted`, `completed`, and `completed_with_gaps` exit successfully; `failed`
exits nonzero. Diagnostics go to stderr.

With another owner, `--wait` polls the common status API with bounded backoff
until terminal; it does not perform competing work. If that owner dies, the
waiting CLI can take over only after lease expiry, using a new fencing epoch.
With **no** owner, the CLI drives the common step API in the foreground until
terminal, with or without `--wait`; it never returns accepted-only and leaves
no hidden daemon.

Stopping a waiting process does not cancel its durable request. If no owner
remains after `accepted`, the request waits for the next GUI/CLI owner.
If an application externally cancels a request while the CLI waits, the CLI
prints a cancellation diagnostic to stderr and exits nonzero without fabricating
a `failed` JSON result or changing the persisted cancelled state.
Applications can diagnose this with
`StorageHandle::thread_index_request_status(request_id, now_ns)`, including its
`progress` and optional `owner`; the CLI does not invent a separate status store.
