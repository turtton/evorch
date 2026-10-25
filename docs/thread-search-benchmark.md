# S8 runtime thread_search benchmark

`crates/runtime/examples/thread_search_measure.rs` is an opt-in, release-only
benchmark, **not** a storage search microbenchmark. It uses a zero-wait scripted
`AgentModel` and the real agent loop, meta dispatch and persisted `RunStore`.
The normal authorization, metadata reads, candidate verification, redaction,
JSON/output bounds and runtime snapshot persistence remain enabled.

## Safety and prerequisites

- Coordinate an exclusive quiet/build/measurement interval with the parent.
  Do not run this concurrently with another performance gate.
- The only initial input is the already-approved W6 backup at the constant
  `BACKUP` in the example. The live/original DB is never opened. SQLite's online
  backup API opens this backup read-only and produces an independent new seed,
  then a new DB for each cell. Existing destinations are rejected.
- Supply a **new** private work directory and an existing canonical project
  directory represented in that backup. No original workspace files are edited.
- Retain all copies for diagnosis; the program does not delete other artifacts.
  On Unix the directory is mode 0700 and DBs mode 0600. Copies contain normal
  runtime-persisted query/result history and must not be published.
- No tracing subscriber is installed. stdout/stderr contain classifications,
  hashes, counts, system pressure and timing only; no query, source text,
  snippets or raw search output. Errors are hashed instead of formatting
  potentially sensitive SQL/serialization details.
- With the approved 2,463,387,648-byte backup, 29 DB copies require at least
  66.5 GiB before index/WAL growth. Preflight requires another 16 GiB of headroom.
  Check actual available space before running; do not remove unrelated files.

After scheduling permission:

```sh
nix develop -c cargo test -p runtime --example thread_search_measure -j4
nix develop -c cargo build --release -p runtime --example thread_search_measure -j4
target/release/examples/thread_search_measure \
  "$TMPDIR/s8-NEW-measurement" /canonical/project/root \
  > "$TMPDIR/s8-NEW-summary.jsonl"
```

Do not enable shell tracing or runtime text logging. A nonzero exit is a failed
gate, not permission to reduce repetitions, broaden provenance, relax limits,
or report a successful latency result.

### Cell scheduling and resume

Long runs can be split without reducing the 100-drain cell size:

```sh
target/release/examples/thread_search_measure "$TMPDIR/s8-results" /canonical/project/root \
  --prepared-seed "$TMPDIR/s8-initial/seed.db" --cell ascii_trigram-false-false
target/release/examples/thread_search_measure "$TMPDIR/s8-results" /canonical/project/root \
  --prepared-seed "$TMPDIR/s8-initial/seed.db" --cell short-true-false
target/release/examples/thread_search_measure "$TMPDIR/s8-results" /canonical/project/root \
  --prepared-seed "$TMPDIR/s8-initial/seed.db" --aggregate
```

Cell IDs are `<class>-<cold boolean>-<backfill boolean>`. The prepared seed is
opened only with read-only connections and the backup API. A temporary
independent probe performs normal database initialization. The manifest hashes
canonical source/authority metadata, scope, query/oracle sets, extractor and
accounting versions, repetitions, warmups, limits and timing definition. A
condition mismatch refuses resume. Completed cells are saved atomically and
skipped, never silently rerun; failed cells remain failed. Aggregate reports all
missing cells and cannot pass unless all 28 are present and below threshold.
If a cell fails, stop and diagnose before spending hours on the rest.

`--import-summary <safe-jsonl>` explicitly imports completed **failed** pre-manifest cells
from the initial run after checking scope/classification, query/oracle hashes,
hit counts and 100 repetitions/5 warmups. The source summary hash and explicit
pre-manifest import classification are retained. This is not permission to
import unrelated measurements or infer a missing cell. A partially measured
cell cannot be imported. No query or output body is stored in the manifest.
Because old summaries lack a binary/conditions fingerprint, an import can
preserve failure evidence but can never grant acceptance.

## Frozen protocol

The 28 cells are the Cartesian product:

| Axis | Values |
| --- | --- |
| Cache | same SQLite read connection, or new `RunStore` read connection per drain |
| Backfill | off / on |
| Query class | ASCII trigram, short ASCII (2 chars), Unicode, phrase, regex, explicit tool kinds, no hit |

Each cell has **100 measured complete cursor drains**. Warm cells first perform
**5 unmeasured drains on the same connection**. Cold cells recreate `RunStore`
and its SQLite read connection before every measured drain, without warmup.
Connection initialization itself is outside timing. SQLite connection cold is
**not OS-cold**: neither page-cache eviction nor dropping global OS caches is
performed or claimed. Cursor continuations stay on the connection/runtime that
issued them; a new connection is used only at the next independent drain.

The per-page timer starts immediately before the script returns a `ToolUse`;
it ends at entry to the next `AgentModel::complete`, before parsing/verifying
the returned `ToolResult`. There is zero model/network wait, but the real
runtime path, persistence and scheduler overhead between those points are
included. The full-drain timer covers first ToolUse through final ToolResult,
including continuation handling between pages. Startup and terminal shutdown
are outside timing. Assertions and background completion barriers do not count
as a page's execution time.

For sorted samples `x`, nearest-rank p95 is
`x[ceil(0.95 * n) - 1]`, so 100 drain samples use the 95th value. First-page,
all-page, and complete-drain p50/p95/max are reported separately. The gate is
strict **all-page p95 < 300,000 microseconds** in every cell; complete-drain
latency is separately reported, not concealed as a single-page result.
2800 measured drains at 300 ms each would already take 14 minutes, excluding
copying, seed indexing, warmups and additional pages. Actual duration must be
reported, and repetitions must not be silently reduced.

## Correctness before speed

Preflight uses the shared atomic snapshot reader and conservative extractor,
never index text or inferred native provenance. It requires exactly:

| Classification | Count |
| --- | ---: |
| All snapshot/note sources | 1857 |
| Indexable within single-step bounds | 690 |
| Pending/unprovable | 1154 |
| Oversized single step | 13 |

The bounds are 512 KiB text and 2048 blocks, inclusive. A changed classification
is a failure to investigate, not an empty-index speed result.

All visible runs of one fixed real project are seeded through the same durable
request, lease, discovery, step, accounting and extractor used by production.
An empty request or missing current index revisions cannot satisfy the seed.
Legacy discovery/proof may supersede a frozen revision-0 request. The harness
then submits a new common request for the current revision, at most four passes;
superseded targets are never rewritten or counted as succeeded.
The oracle uses canonical project threads, current root/delegated run membership,
metadata and shared extraction from canonical snapshots. It independently
matches queries and compares the entire **thread-ID → hit-count** map after
cursor drain, not only a few capped snippets. Final-report metadata is excluded
from these fixed query kinds to avoid making the oracle depend on production's
metadata/conversation final-report de-duplication; title, user and note metadata
are included. Tool kinds are explicitly enabled only for the tool cell.

Queries are selected deterministically from redacted, indexable visible source
material before timing and identified only by SHA-256. Every positive class
must have canonical hits; the fixed no-hit sentinel must have none. `partial`
requires continuation, repeated cursors fail, and every drain must exhaust its
cursor before equality can pass. Unauthorized threads necessarily fail the
identity-map comparison. Saved benchmark callers are unbound Worker roots:
trusted project authority comes from `RunConfig`, not persisted text, and the
callers cannot become conversation-thread candidates.

## Backfill and environment evidence

Each on-cell starts from its own copy of the same prepared seed. Only real
other-project request targets are backfilled; overlap with the visible scope
is rejected. Preflight records other-project and indexable counts, requiring
at least 100 indexable targets. Each measured drain triggers common foreground
CLI stepping on a separate OS thread at its first ToolUse. A completion barrier
outside timing requires a successful target increase and actual interval
overlap, not a sleep or a static “backfill enabled” label. Exhaustion, no progress,
lease loss, pause, or no overlap fails the cell. A progress record states whether
the commit was observed only after the search returned.

Reports include pending/succeeded progress for every repetition, index
steps/commits/failures/evictions/pauses/merges, capacity status, DB/WAL/SHM sizes,
and Linux CPU/IO/memory pressure plus load before/after each cell. No checkpoint
or WAL truncation is injected into a measured interval.

## Current validation status

The first real-copy release cell (ASCII trigram, warm, backfill off) completed
100 drains / 800 pages with an exact canonical set of 95 threads / 1166 hits:

| Metric | p95 |
| --- | ---: |
| All pages | **1589.601 ms — FAIL** |
| First page | 1032.397 ms |
| Full cursor drain | 6590.899 ms |

Maximum output was 16,312 bytes and no page was partial. Strict classification
and nonempty current seed coverage passed. The other 27 cells are **not
completed**, not passing. Measurement stopped after this actionable failure;
the partly started next cell is not counted. This is evidence of an unmet
latency target, not authorization to relax the threshold.
The recorded host pressure had near-zero CPU/memory pressure but high IO
pressure (`some avg10` 82.78% → 80.62%, `full avg10` 79.37% → 77.11%).
The off-cell made zero index steps/commits; capacity remained normal.
Attribution must distinguish actual search reads from runtime persistence and
unrelated host IO before proposing an optimization.
CPU PSI is a stall/pressure measure, **not CPU utilization**; a zero value does
not rule out CPU work such as matching or redaction.

After the separate scope-first SQL fix (`9d741ac`), a new binary/conditions
manifest and independent copy of the **same prepared seed** repeated the same
100-drain warm/off ASCII cell without changing scope, query or threshold:

| Metric | p95 after scope-first SQL |
| --- | ---: |
| All pages (800 pages) | **385.323 ms — still FAIL** |
| First page (100 samples) | 391.572 ms |
| Full cursor drain (100 samples) | 1418.988 ms |

The complete canonical map remained identical (95 threads / 1166 hits) for
all 100 drains. Partial pages remained zero; maximum output remained 16,312
bytes. This is a substantial improvement, **not a passing S8 result**. The
other 27 cells remain unmeasured. The off-cell still made zero index
steps/commits, capacity stayed normal, and WAL ended at 4,268,352 bytes.
IO PSI `some avg10` was 82.16% → 80.10%, `full avg10` 79.33% → 76.55%.

After the separate narrow-header/lazy-summary optimization (`8471343`), another
new binary/conditions manifest repeated that same seed/scope/query cell:

| Metric | p95 after SQL and metadata changes |
| --- | ---: |
| All pages (800 pages) | **177.771 ms — this cell passes** |
| First page (100 samples) | 180.859 ms |
| Full cursor drain (100 samples, separately reported) | 661.612 ms |

All 100 drains retained the exact same canonical 95-thread / 1166-hit map,
zero partial pages and the 16,312-byte maximum output. The five same-connection
warmups and 100 repetitions were unchanged. Capacity was normal; index
steps/commits remained zero; WAL ended at 4,243,632 bytes. IO PSI `some avg10`
was 76.80% → 77.77%, `full avg10` 75.30% → 75.47%. This **does not establish an
S8 pass**: the other 27 cells must still complete under the same binary manifest.
The full drain is not subject to the individual-request 300 ms threshold.

Resuming that manifest completed warm/off short and Unicode cells, but stopped
automatically at the warm/off phrase cell:

| Warm/off class | Pages / 100 drains | Canonical threads / hits | All-page p95 |
| --- | ---: | ---: | ---: |
| Short | 900 | 110 / 2221 | 85.735 ms |
| Unicode | 700 | 68 / 962 | 87.109 ms |
| Phrase | 200 | 8 / 10 | **737.732 ms — FAIL** |

Every complete map matched the oracle for every drain. Short searches produced
100 partial pages with valid continuations, all drained successfully. Phrase
first-page p95 was 740.006 ms; full-drain p95 was 760.821 ms, with zero partial
pages and an 8,276-byte maximum output. The aggregate therefore remains
**3 passing cells, 1 failing cell, 24 missing**, not an S8 pass. No cold or
backfill-on cell has been measured yet.

`--diagnose` performs three same-connection passes over the same visible
project's public read APIs and one supplementary real-loop drain. It emits only
counts, canonical source byte sizes and timings. Existing meta tool lifecycle
timestamps split the inclusive page time into before dispatch / dispatch /
after dispatch; event input/output payloads are never written to the report.
This attribution is explicitly **not an acceptance measurement** and cannot
replace the 100-drain E2E cells.
`--diagnose-class phrase` selects the frozen phrase query for attribution;
plain `--diagnose` retains the ASCII default. Unknown classes fail closed.
Phrase filters keep the entire phrase as one literal, as production does;
word queries split terms, and regex candidate replay uses the unfiltered route.
The supplementary plan reproduces the current scoped storage SQL only for a
single trigram alternative, Text kind, first replay batch, `after=None` and
limit 200; it records no bound values and is not an internal production trace.

The extended diagnostic also captures each real page's returned thread/hit
counts, index coverage, partial/cursor flags and bounded output size. It keeps
the actual input cursor memory-only, inspecting its payload to recover the
scanned thread position and optional resumed run/block. This does **not** verify
the token signature or grant authorization; production still authenticates and
re-authorizes every call. It replays the public candidate API over that
100-thread preload window (plus an optional resumed thread), recording batch/scope hashes, keyset
positions, SQL calls/rows/bytes, route classification and duplicate candidates
across pages. Redaction uses the production `SecretRedactor` settings; the
private runtime matcher cannot be reused, so its RegexBuilder flags and 1 MiB
limits are reproduced and explicitly marked as a diagnostic approximation.
The replay does not emulate early hit/output-byte stops; its counts are
overfetch upper bounds, **not a trace of production's internal SQL calls**.
Do not subtract unrelated percentiles or equate replay time with E2E time.
Earlier diagnostic versions inferred cursor starts from the last returned hit.
This is not exact for rare queries that scan more threads than they return;
their non-first-page replay windows must be treated as estimates. The current
replay uses the emitted scanned-position payload, not the last hit. This
correction does not affect actual runtime timings or canonical equality checks.

Supplementary attribution on the same prepared seed found:

- A representative real drain took 6.545 s; lifecycle timestamps place over
  99% of each page inside dispatch. Before/after dispatch together were only
  a few milliseconds, so snapshot persistence did not explain the failure.
- The query was classified by the shared router as **trigram**.
- Over the replayed actual cursor windows, literal candidate SQL took
  8.876 s / 61 calls, versus full-scan SQL 52.069 ms / 92 calls. Both produced
  the same 2019 matching candidates including repeats; the literal replay
  had 1113 cross-page duplicate candidates. Redaction took 20.202 ms, and the
  reproduced matcher took 46 µs. These replay totals are not E2E percentiles.
- The final three windows returned **zero** indexed candidates but literal SQL
  still took 875.958 / 431.600 / 143.103 ms; full scans took only
  1.390 / 0.700 / 0.203 ms. This discriminates SQL routing/plan amplification
  from output formatting or matcher cost.
- A read-only `EXPLAIN QUERY PLAN` with the same bound SQL shape and query,
  over a supplementary 71-run unindexed scope, selected the global
  `conversation_blocks(kind)` index and FTS EXISTS before scanning scope and
  looking up current run coverage, then used a temporary ORDER BY B-tree.
  This is not a captured production call trace; the synthetic scope is stated
  explicitly. Production SQL/API changes belong to a separate implementation
  task, with the 300 ms gate still unmet.

A second supplementary diagnostic after the scope-first SQL fix observed a
384.910 ms first-page dispatch. Public reads over its same 100-thread window
loaded 288.545 MB of canonical snapshot JSON: `terminal_summaries` took
248.995 ms and membership reads 37.122 ms in one of three read passes.
Candidate SQL replay took 158.886 ms for that window (its candidate counts are
overfetch upper bounds, not a latency bound); redaction was 11.007 ms. The final
three windows had no indexed candidates: membership plus summary reads took
87.202 / 49.771 / 14.210 ms,
close to their real dispatch times 88.702 / 50.964 / 15.589 ms, while candidate
SQL was only 1.432 / 0.721 / 0.205 ms. This isolates repeated giant-JSON summary
reads as the remaining large baseline cost. The timings are separate public
read passes, not an internal trace, so their sums are not an exact decomposition
of a production page. A narrower metadata projection or request-local reuse
requires a separate production change and fresh E2E validation.

Supplementary phrase attribution after both production changes observed
741.363 ms first-page dispatch and 18.970 ms continuation dispatch, retaining
the canonical 8-thread / 10-hit map. The phrase is **one whole trigram-routed
literal**, not multiple word terms. Over the exact first 100-thread preload
window, literal candidate SQL took 666.438 ms / 12 calls / 4 rows / 7,098 bytes;
full-scan SQL took 19.713 ms / 25 calls / 3,302 rows / 7.086 MB. Both yielded
the same four matching conversation candidates; the other canonical hits are
metadata. Header/membership reads were 60.344 ms; literal redaction/matcher
took 24 / 11 µs. The plan retained scope-first materialization and the
`(kind, run_id)` block lookup, but still evaluated the FTS EXISTS per scoped
block, followed by a temporary ORDER BY B-tree. The phrase diagnostic's old
continuation replay inferred the start from a last hit and was not exact;
only its first-window SQL comparison is used as evidence. A subsequent
algorithm change needs separate production ownership and fresh measurements;
global FTS evaluation before authorization scope is not an acceptable shortcut.
