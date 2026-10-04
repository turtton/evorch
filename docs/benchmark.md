# Agent benchmarks

The benchmark runner helps choose a model for an evorch role/category using
recorded delegations from a complete task. It uses the normal agent runtime,
prompts and tools. It is an opt-in CLI workflow; Arena is no longer registered
as a default GUI tab.
The former automatic `arena-main` tab is also removed when a saved layout loads.

## Usage

Configure the baseline role/category bindings and provider credentials as for a
normal evorch run. Record the example task into a new directory:

```sh
cargo run -p evorch -- benchmark record \
  --spec examples/benchmark/clamp.json \
  --output /tmp/clamp-benchmark \
  --user-config /path/to/evorch-config
```

Replay the recorded delegation with an explicitly selected provider profile
and model. Replace the example profile and model names with configured values:

```sh
cargo run -p evorch -- benchmark replay \
  --record /tmp/clamp-benchmark \
  --trial candidate-1 \
  --profile my-provider \
  --model my-model \
  --user-config /path/to/evorch-config

cargo run -p evorch -- benchmark report --record /tmp/clamp-benchmark
```

These commands make real provider requests. Creating the baseline is a full
task execution; replay runs only the recorded delegation. The example caps
tokens, tool calls and elapsed time. Choose limits appropriate to the configured
models and task. No paid provider requests are needed for the offline tests.
The token limit sums reported input and output across the whole execution,
including repeated input and cached input; it is not an output-token limit.

The JSON spec contains `fixture_dir` (relative to the spec), `root_prompt`,
`target` (`role`, optional `category`, and one-based `occurrence`), `verifier`
(`program`, `args`, `protected_paths`, `allowed_changed_paths`), and `budget` (`max_tokens`,
`max_tool_calls`, `timeout_seconds`). The example captures the first
`worker/quick` delegation. Verifier paths are relative to the fixture and must
identify existing files; they are preserved as the evaluation contract.
The target must be a self-contained leaf role, with an optional public category
owned by that role. Changes must be confined to the exact files in
`allowed_changed_paths`.

## What the comparison measures

A baseline completes the task and records a selected delegation's initial
context, tools and workspace. Candidate trials repeat that delegation with a
different model. The baseline's later output, reviews and final verification
are evaluation evidence, not input to the candidate.

Compare the assigned work, output, changed files, verification results and
local usage. A successful baseline is not a gold answer: later agents may have
repaired the selected agent's mistakes, and different solutions may also be
valid. Accept a candidate against the assigned work's requirements rather than
requiring its output to match the baseline.

Local replay does not measure whether the complete task would succeed or cost
less with the new model. Replaying several recorded delegations evaluates each
in the state produced by the baseline. Their scores cannot be added together
to predict replacing a role/category throughout a new execution. Checking the
effect on later delegation, review and repair requires a separate complete
execution with the selected configuration.

## Reproducible trials

The fixture is copied into a runner-owned workspace. Trials run sequentially
at the same absolute workspace path so recorded paths and system instructions
remain valid. Before each replay, the workspace is restored to its recorded
starting state. The user's original fixture is never the agent's working copy.
Snapshots preserve file bytes, directory names and Unix permission bits.
Ownership, timestamps and extended attributes are outside the snapshot format;
use fixtures whose behavior does not depend on them. Git attribute files and
special files such as FIFOs or sockets are rejected before capture or restore.

Recordings and baseline evaluation evidence belong outside the candidate's
working tree. Each trial uses a fresh runtime and event stream. The saved
role/category, conversation, tool schemas and generation settings define the
comparison; the candidate selection changes the provider/model. Unsupported
settings or execution states must be reported rather than silently approximated.
The initial version requires baseline and candidate to use the same API protocol
(for example, OpenAI Chat Completions). Explicit settings that the protocol cannot
send are rejected. Provider profiles can differ within that protocol.

The first version targets sequential, self-contained delegations. A recording
that needs a live parent, concurrent agents, pending shell jobs or team state
is not a portable local trial. Startup project rules and loaded skills are
frozen with the recorded input. Dynamic rule discovery, skill loading, manual
compaction and parent-dependent actions are unsupported in a recorded leaf.
Thread-bound goals and their continuation/completion checks are unsupported in
benchmark runs.
Recorded tool descriptions reflect the baseline model's tool interface and
remain fixed even when comparing a different model family.
Shell commands run with private process and network namespaces. Asynchronous
or interactive shell execution is unsupported. Fixture and trial workspaces
must contain no Git metadata, including nested `.git` directories or files.
The `git_diff` tool is unsupported in benchmark runs; reports compute diffs
from the runner-owned snapshots instead.
An interrupted run leaves its output and lease file for explicit inspection;
do not remove `active.lock` until the owning process has stopped.
If an agent is interrupted, errors after starting, or exceeds the benchmark
budget, its record is marked unusable. Its output remains available for
inspection, but no snapshot or verifier result is accepted and further replay
requires a new recording. A provider selection rejected before an agent starts
is recorded as a failed trial without invalidating the record.

Model sampling and provider caches can still
affect repeated results. Keep actual usage and cache information with the
results, and use several representative tasks before adopting a configuration.

## Example task

`examples/benchmark/clamp` is a small Rust fixture with an intentionally
incorrect inclusive clamp implementation. Only `src/lib.rs` is the repair
target. The acceptance tests in `tests/clamp.rs` exercise in-range values,
boundaries, negative and extreme values, and rejection of reversed intervals.
The unmodified fixture is expected to fail one acceptance test.

The verifier is `cargo test --offline`. The example pins Rust 1.97.0, matching
the repository; install that toolchain before running it because the isolated
shell cannot download a missing toolchain. Keep the original acceptance tests
protected during evaluation so a candidate cannot obtain a passing result by
changing the test expectations. Verification uses a fresh original fixture with
only the allowed output files copied in, so trial-created Cargo configuration
and build outputs cannot substitute for the acceptance tests.
A passed verifier is evidence about this
assigned repair, not a general model ranking.

Reports contain deterministic verifier results, diffs, usage and baseline
downstream evidence for human evaluation. They do not compute an LLM judgment,
model ranking or a monetary cost estimate.
