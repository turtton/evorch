# CLI tools that stay outside the core (PR 17e, ADR 0045)

`evorch maintenance <command>` (offline), `evorch inspect` and `evorch benchmark`
do not go through the core. They keep their arguments and exit codes. `--db` of
`maintenance` and `inspect` stays (the commands `eval`, `reindex-threads` and
`maintenance backfill-events` move to `--user-config` in PRs 17c and 17d, not here).

## Offline maintenance and a running core

A core holds the database lease (`<canonical db path>.maintenance-lock`) shared, through
`shared_with_wait`, for as long as its writer lives. Every offline command takes
the same file exclusively, without waiting, and keeps it until it returns (the
`_lease` binding in `storage::maintenance`); preview needs the file to exist and locks it
the same way. So a running core, or any other writer, makes every offline command fail
before it reads or writes anything: no backup, no sidecar, no new lock file.

- The lease path is built from the canonical path, so a symlink to the file, a symlink to its
  directory and `dir/./..` spellings all name the same lease.
- A refused or failed command releases the lease when it returns, and a crash or `kill`
  releases it with the process (it is an advisory lock on an inode that is never unlinked).
- While an apply is inside its critical section, a core, a writer, another offline command,
  an inspection and a plain SQLite reader are all kept out (the lease, plus SQLite's
  exclusive lock that apply takes), and when it ends or is killed the lease is free again
  and the database file is as it was.
- A hard link to the database file is *not* an alias: it canonicalizes to another path and
  therefore another lease file. SQLite itself does not support hard links to a database
  (WAL sidecars are named after the path). Do not point `--db` at one.
- Tests: `crates/evorch/tests/maintenance_e2e.rs` (`live_core`). The in-progress tests run
  `compact-events --apply` as a child process and point `--sidebar` at a named pipe: the
  command reads the sidebar after it has taken both locks, and opening the pipe for writing
  returns exactly when the child has opened it, so the test knows the child is inside the
  critical section without sleeping. Closing the pipe lets the child finish; `kill` stops it.

## Inspect

`evorch inspect` opens an existing database read-only through
`Database::open_for_inspection`: it never creates or migrates a database, takes the lease
shared (so a core is never blocked, and offline maintenance is refused while it runs) and
never writes the database file. SQLite's own reader protocol may create the `-wal` and
`-shm` files of a WAL database.

`open_for_inspection` would create a missing lease file (`storage::inspect::open`, outside
the 17e scope). The command therefore refuses a database that has no lease file, before
opening anything, and names the way out: start current evorch once and exit cleanly, or
create the empty file (mode 0600). A copy of the database alone needs that once.

- Tests: `crates/evorch/tests/inspect_e2e.rs`.

## Benchmark

`evorch benchmark` composes a runtime without a store: it never opens the settings database
(`<user-config>/evorch-events.db`) or any file next to it. `record` also refuses an output
directory that is that database, a file beside it (`evorch-events.db*`: sidecars and lease
files), a path under it, or a place that contains it, with aliases (symlinks, `..`) resolved
and before anything is created; such a directory would keep the next core from creating its
database.

The tests measure open attempts from outside: a sentinel file stands at the database path
with equal, old access and modification times; reading it moves the access time and writing
it moves the rest, so an unchanged stamp means nothing opened it. A control test shows that
a real `Database::open` is detected. This needs a filesystem that updates access times
(`relatime`, the default); on a `noatime` mount the control test fails instead of the
measurement passing silently.

Known side effects, kept on purpose:

- A provider that uses OAuth writes a refreshed token back to the credential store, like a
  normal run does.
- When no keychain is available and `<user-config>/credentials/credentials.json` does not
  exist, the file credential store creates the directory (mode 0700) and an empty file
  (mode 0600). An existing credentials file is not rewritten (the test asserts its bytes
  are unchanged), but the store sets the permissions of an existing directory and file to
  0700 and 0600 each time it opens.

- Tests: `record_never_opens_the_settings_database_or_rewrites_credentials`,
  `record_refuses_an_output_at_the_settings_databases_place` and the sentinel control in
  `crates/evorch/tests/benchmark_e2e.rs`.
