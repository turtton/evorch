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
- A refused or failed command releases the lease when it returns, and a crash releases it
  with the process (it is an advisory lock on an inode that is never unlinked).
- A hard link to the database file is *not* an alias: it canonicalizes to another path and
  therefore another lease file. SQLite itself does not support hard links to a database
  (WAL sidecars are named after the path). Do not point `--db` at one.
- Tests: `crates/evorch/tests/maintenance_e2e.rs` (`live_core`).

## Inspect

`evorch inspect` opens an existing database read-only through
`Database::open_for_inspection`: it never creates or migrates a database, takes the lease
shared (so a core is never blocked, and offline maintenance is refused while it runs) and
never writes the database file. SQLite's own reader protocol may create the `-wal` and
`-shm` files of a WAL database. If the lease file does not exist yet, inspection creates it,
as a starting writer would.

- Tests: `crates/evorch/tests/inspect_e2e.rs`.

## Benchmark

`evorch benchmark` composes a runtime without a store: it never opens the settings database
(`<user-config>/evorch-events.db`) or any file next to it. The test places directories at
those paths, so an attempt to open them fails the run.

Known side effects, kept on purpose:

- A provider that uses OAuth writes a refreshed token back to the credential store, like a
  normal run does.
- When no keychain is available and `<user-config>/credentials/credentials.json` does not
  exist, the file credential store creates the directory (mode 0700) and an empty file
  (mode 0600). An existing credentials file is only read; the test asserts its bytes
  are unchanged.

- Test: `record_never_opens_the_settings_database_or_rewrites_credentials` in
  `crates/evorch/tests/benchmark_e2e.rs`.
