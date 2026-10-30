# Conversation index and reindexing

The GUI explicitly starts the shared storage index owner. Merely opening a
`Storage` does not start indexing. GUI and CLI use the same durable request,
lease, and step APIs; there is no separate CLI indexing engine.

Search keyset-scans the indexed body within visible runs and does not read FTS;
matching happens in the runtime after redaction
([#374](https://github.com/turtton/evorch/issues/374),
[PR #432](https://github.com/turtton/evorch/pull/432)).

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
