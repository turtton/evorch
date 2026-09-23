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
The operator answered **「ADRを見直し、復元範囲の拡大も検討する」**. The accepted
scope is recorded in [the restore proposal](restore-contract-proposal.md).
The operator subsequently answered **「OK。承認する」** to the concrete ADR
revision and finalization correction. Both answers were recorded with the CLI;
canonical ADR 0027 and its related feature summary now reflect the approved scope.
The earlier automated approval rejection was resolved by this explicit answer.
No approval bypass or hand-edited workflow state is used.

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

The initial clarification and final explicit approval were recorded:

```sh
intent-cli interview record-answer --session harness-reliability-20260923 \
  --question restore-scope --domain evorch \
  --prompt 'ADR 0027が禁止しているDynamicTeam／team関連状態の復元は、既存契約を維持するか、ADRを見直して復元範囲の拡大も検討するか？' \
  --from-file /tmp/evorch-restore-answer.txt --write --format markdown
```

Final approval was previewed and then recorded with:

```sh
intent-cli interview record-answer --session harness-reliability-20260923 \
  --question final-approval --domain evorch \
  --prompt '復元改訂案に沿ってADR 0027を正式改訂し、shell回収・最終保存・ハンドル解放と引継ぎ準備・元run終了通知・昇格先root開始の順序で終了処理を修正する。既存通知順序を維持し、ハンドル枯渇と即時再開の競合を解消して再検証する、この2点を承認して続行してよいですか？' \
  --from-file /tmp/evorch-harness-final-approval.txt --write --format markdown
```

Artifact: `intents/evorch/interviews/harness-reliability-20260923.json`.
Each input file contains the operator's exact answer. The operator also explicitly
authorized merging to main, pushing main, and verifying CI after implementation.

## Files changed

- `crates/tools/src/tools/shell*`, executor/tool APIs: bounded interactive jobs and lifecycle.
- `crates/sandbox/src/exec.rs`: deterministic executable/cwd preflight.
- `crates/runtime/src/meta*`, role capabilities: concrete schemas, bounded event waits and questions.
- `crates/runtime/src/restore*`, `runtime/chat_restore*`: approved history renewal and diagnostics.
- `crates/runtime/src/agent_loop*`, compaction: safe completion, tool intent and context estimates.
- `crates/storage/src/*`, migration v9: bounded durable questions and explicit inheritance.
- `crates/event-bus/src/*`: typed progress/question events and existing projections.
- `crates/gui/src/*`: question cards, answer acknowledgements and restore/context diagnostics.
- Provider observation tests: deterministic scoped-tracing and concurrent ID coverage.
- Corresponding regression tests, `scripts/check-harness.sh`, these documentation files.
- ADR 0027 and its durable-execution feature summary: approved root history renewal boundaries.
- Accepted interview artifact above. No queue-state, label or publish artifacts changed.

## Issue URLs created or updated

None.

## Clarifications opened, answered or deferred

- Restore scope: initial reconsideration and exact final revision both approved and recorded verbatim.
- Shell finalization: concrete correction explicitly approved; notification-order and storage-failure regressions corrected without weakening expectations.
- Delivery: merge, push to main, and CI verification explicitly authorized.

## Skipped commands and reasons

- `intent draft-from-interview --write`: this changes an existing approved ADR, not a new intent shape draft.
- `packet draft --write`, `issue publish-flow --write`: this task directly authorizes
  implementation; publishing a new issue/packet was not requested.
- Queue/label transitions: no published unit is being transitioned.
- `intent-cli run` / model launch: prohibited and unnecessary.

## Forbidden workflow sources not consulted

`intents/rules/**`, copied workflow prompts, and local skills that restate the
intent-cli workflow were not used. CLI help and guides supplied the workflow.
Canonical feature/ADR documents and application prompt fixtures were reviewed
as project requirements and code, not as alternative workflow instructions.

## Final validation

Workspace tests: 3,439 passed, 0 failed, 48 environment-dependent tests ignored.
Workspace, GUI browser and event-bus otel Clippy all passed with `-D warnings`.
See [the validation report](harness-validation.md) for targeted coverage and
reproduction. The final operator report records the pushed main commit and CI run.
