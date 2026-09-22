# Intent-work audit: harness reliability (2026-09-23)

- Domain: `evorch`
- Target repo: `turtton/evorch`
- Implementation: isolated `codex/harness-reliability` worktree.

## Alignment and clarification

The implementation preserves ADR 0001's flexible delegation, ADR 0002's role and
approval boundaries, ADR 0003's stable prompt/tool prefix, ADR 0010's typed core,
and ADR 0026's existing execution/task substrate. Provider transport retries stay
in the provider layer. Bounded event waits and question notifications use the
existing event bus, storage and runtime rather than a parallel scheduler.

One substantive conflict was found: ADR 0027 explicitly rejects DynamicTeam,
team-related and memory/finding configuration at every restoration entrance.
The operator answered **「ADRを見直し、復元範囲の拡大も検討する」**. The narrow,
tested candidate is [the restore proposal](restore-contract-proposal.md).
Canonical ADR 0027 is unchanged pending approval of that exact revision.
Automatic approval review rejected rewriting it on the strength of “consider
expansion” alone; the answer itself was recorded separately. No approval bypass
or hand-edited workflow state is used.

## Read-only commands invoked

CLI version: 0.26.0. Both implementation help and documented guides were checked.

- `intent-cli --help`
- `intent-cli guide commands list`
- `intent-cli inspect --help`
- `intent-cli inspect --domain evorch --target-repo turtton/evorch --format markdown`
- `intent-cli intent --help`, `intent search --help`, `intent explain --help`
- `intent-cli intent status --domain evorch`
- `intent-cli intent search --domain evorch <query>` for `非同期`, `復元`, `承認`
- `intent-cli intent explain --domain evorch v07-durable-execution-substrate`
- `intent-cli guide intent-work setup --help` and `--kind intent-shape`
- `intent-cli guide intent-work audit --help` and `intent-cli guide intent-work audit`
- `intent-cli guide model --format markdown`
- `intent-cli guide onboarding --format markdown`
- `intent-cli automation summary --domain evorch --format json`
- `intent-cli intent next-slice --dry-run --domain evorch --target-repo turtton/evorch --format json`
- `intent-cli interview record-answer --help` and a preview without `--write`.

The binary's supported `record-answer --session --question --from-file` form was
used instead of the stale `--question-id/--answer` example in the guide, consistent
with the repository's verified-interface ledger.

## Mutation commands invoked

Only the already accepted clarification was recorded:

```sh
intent-cli interview record-answer --session harness-reliability-20260923 \
  --question restore-scope --domain evorch \
  --prompt 'ADR 0027が禁止しているDynamicTeam／team関連状態の復元は、既存契約を維持するか、ADRを見直して復元範囲の拡大も検討するか？' \
  --from-file /tmp/evorch-restore-answer.txt --write --format markdown
```

Artifact: `intents/evorch/interviews/harness-reliability-20260923.json`.
The input file contains the operator's exact answer quoted above.

## Files changed

- `crates/tools/src/tools/shell*`, executor/tool APIs: bounded interactive jobs and lifecycle.
- `crates/sandbox/src/exec.rs`: deterministic executable/cwd preflight.
- `crates/runtime/src/meta*`, role capabilities: concrete schemas, bounded event waits and questions.
- `crates/runtime/src/restore*`, `runtime/chat_restore*`: recovery proposal and diagnostics.
- `crates/runtime/src/agent_loop*`, compaction: safe completion, tool intent and context estimates.
- `crates/storage/src/*`, migration v9: bounded durable questions and explicit inheritance.
- `crates/event-bus/src/*`: typed progress/question events and existing projections.
- `crates/gui/src/*`: question cards, answer acknowledgements and restore/context diagnostics.
- Corresponding regression tests, `scripts/check-harness.sh`, these documentation files.
- Accepted interview artifact above. No queue-state, label or publish artifacts changed.

## Issue URLs created or updated

None.

## Clarifications opened, answered or deferred

- Restore scope: answered with permission to reconsider; recorded verbatim.
- Exact canonical ADR revision: deferred until candidate review and final operator approval.

## Skipped commands and reasons

- `intent draft-from-interview --write`: the existing ADR revision is still a candidate.
- `packet draft --write`, `issue publish-flow --write`: this task directly authorizes
  implementation; publishing a new issue/packet was not requested.
- Queue/label transitions: no published unit is being transitioned.
- `intent-cli run` / model launch: prohibited and unnecessary.

## Forbidden workflow sources not consulted

`intents/rules/**`, copied workflow prompts, and local skills that restate the
intent-cli workflow were not used. CLI help and guides supplied the workflow.
Canonical feature/ADR documents and application prompt fixtures were reviewed
as project requirements and code, not as alternative workflow instructions.
