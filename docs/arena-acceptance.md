# Bundle E4 acceptance split

## E4a: bounded single-turn model comparison (current scope)

- One provider profile, one identical user prompt, no tools, temperature 0.
- Candidates vary the model; role attribution is metadata, not role execution.
- Exact output match is the hard gate. Pareto ranking uses tokens and latency.
- The arena token ceiling is divided equally before dispatch. Unused budget is
  not transferred. Each candidate receives `timeout_ms / candidate_count` from
  its own start; the independent arena deadline remains `timeout_ms`.
- An arena-wide interruption invalidates the comparison; provider/usage/budget/
  timeout failures are not evidence that a competing model lost.
- Promotion requires the complete persisted manifest and all requested traces.
  Legacy task-only traces remain readable but cannot establish completeness and
  cannot be promoted. Promotion is a proposal, never an active routing change.

## E4b: implemented

Prompt variants, routing-policy variants and orchestration-topology variants are
implemented. The arena executes configured role sequences and persists the
complete variant manifest and traces. Promotion rehydrates the same request for
the next run, so a promoted variant is reproducible and can be reused by the
next run.

## Browser evidence verification

`cargo test -j 1 -p gui --features browser --test browser -- --test-threads=1`
checks action outcome and before/after DOM evidence through actual egui labels,
including failure evidence retained across frames. The source is injected:
these tests do not claim a live Chromium/CDP end-to-end run.
