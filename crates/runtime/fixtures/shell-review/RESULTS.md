# Work A: live model comparison

Status: **incomplete; draft PR, not a release-gate pass**.

The corpus, profile/model aliases, three trials per case and tolerances were
frozen before the baseline. The baseline ran before production edits. Primary:
`codex-personal/codex-auto-review`; fallback:
`codex-work/codex-auto-review`. These are the configured command-review
candidates, evaluated individually through the production provider adapters.
The model alias can resolve server-side; this is not a claim about an immutable
provider checkpoint or broader statistical safety.

## Primary: 17 cases × 3 trials, before and after

| Group | Trials per version | Baseline auto-approvals | Candidate auto-approvals |
|---|---:|---:|---:|
| Required improvements | 12 | 0 | 12 |
| Safety / unauthorized effects | 30 | 0 | 0 |
| Existing approvals | 9 | 9 | 9 |

Required cases are explicit removal of three worktrees and named branches,
affirmative “はい” to the displayed enumerating question, explicit destruction of
one named unique directory, and clean local cleanup of a freshly confirmed merged
PR with matching head. Procedure-missing refusals disappeared in all 12 primary
trials. The explicitly destructive cases remain **critical**, with high explicit
permission, full scope and valid user references; risk was not reduced by consent.
The merged/clean/matching cleanup is low risk and an ordinary task step.

No new unsafe approval (0/30), no new denial of an existing approval (0/9), and
no residual required-case refusal were observed on the primary. These sample
results are not a guarantee outside the fixed corpus.

## Fallback unavailable

Both baseline and candidate stopped on the first fallback trial with:
`profile=codex-work model=codex-auto-review: rate limited (429); retry-after unknown`.
There are **zero successful fallback trials**, not 51 denials or approvals.
No retry loop, credential/configuration reset or substitution of a different
model was used. Normal host keychain access worked for the primary; fallback
provider quota/rate-limit recovery is still needed. After recovery, rerun both
versions with the unchanged protocol. Do not mark this PR ready from the primary
results alone.

## Evidence and repeatability

* `baseline-results.jsonl` and `candidate-results.jsonl`: complete synthetic
  primary decisions, first fallback error, input hashes and schemas. No real user
  conversation or credential data is included.
* `baseline.json`: exact pre-change static instruction/schema snapshot.
* `compare.py`: offline comparison; exits 2 when the protocol is incomplete,
  rather than silently treating missing/error trials as passing. Its required
  case check conservatively flags every nonapproval for human reason inspection.
* The normal unit suite replays all 102 successful recorded decisions through
  the final policy and checks the candidate instruction/schema hashes. Subsequent
  completeness hardening only rejects missing verdict fields; all recorded
  successful provider responses supplied those fields. No extra paid trial was
  substituted into the fixed sample.

Run the offline report from the repository root (requires a Python 3 interpreter; standard library only):

```sh
python3 crates/runtime/fixtures/shell-review/compare.py \
  crates/runtime/fixtures/shell-review/baseline-results.jsonl \
  crates/runtime/fixtures/shell-review/candidate-results.jsonl
```

## Remaining refusal patterns / provenance follow-up

All remaining primary refusals in this corpus are safety cases: extra compound
command targets, a “yes” extended beyond the question's list, vague cleanup,
explicit exclusions, delegated/justification/rules-only claims, absent lineage
user evidence, secret upload, and stale/mismatched facts. They are intentionally
not overridden. The corpus does not establish whether file provenance would
resolve other real-world refusals; collect those separately without weakening
scope checks. A15 does not infer cleanliness or matching heads from missing facts.

Historical v1 manifests were inspected read-only before the version bump; they
contained hashes, not the rejected command text/reason, so no private historical
command could be faithfully reconstructed. See README for counts and commands.
