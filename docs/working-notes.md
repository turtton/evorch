# Working notes and original evidence

The public `ledger_append` / `ledger_read` tools are replaced by
`working_note_write`, `working_note_read`, and `history_read`. The internal
`run_ledger` table and `RunLedgerAppended` event retain immutable revision history
for inspection and the GUI.
This pre-v1 tool-schema change starts a new provider cache baseline when upgrading
from the old tool set. Within the new version, tool definitions remain stable;
ordinary turns and post-boundary reuse are covered by the wire cache contracts.

## Why change the ledger

The 2026-10-15 retained-history audit found 531 ledger entries across 170 runs.
Of 35 explicit reads, 30 were empty; all 15 Orchestrator reads were empty.
The five nonempty reads repeated the caller's own earlier entries, with no
compaction between the write and read. Automatic restore injection frequently
repeated already visible text and could repeat obsolete PR or permission state.
In one worker escalation, the receiver attempted to read the source's sequences
but read its own empty ledger instead.

These observations do not prove that notes cannot help. They show that append
history and optional self-scoped reads did not establish useful continuity.
The new contract supplies current state at the boundaries where it is needed and
provides bounded access to original evidence.

## Model-facing contract

`working_note_write({"body": "..."})` replaces the caller's current note.
An empty body explicitly clears it. Each successful write creates a new revision;
the limit is 8 KiB and the existing secret guard still applies.
`working_note_read({})` returns the latest revision or `null`, rather than all
previous text. A cleared revision remains identifiable by its empty body.

Useful note contents are non-obvious findings, decisions and rejected approaches
with reasons, unresolved questions, evidence references, and the next action.
Goals, procedures, user authorization, and operation outcomes stay in their
existing runtime records. A working note is a model-authored claim, including
when it quotes a user or says an operation succeeded.

After successful compaction or a role-chain context boundary, the selected note
is presented once as `[working-note]` plus its revision metadata. Later requests
retain that provider input prefix. Full-history restoration does not replay the
note separately. An interrupted `/continue` retries the saved provider input
without adding a note, an empty message, or a continuation prompt.

Escalation freezes the source's selected note and original-history access in the
durable destination seed. Admission failure, stop, and restart reuse this seed.
Later source writes cannot change the receiver's inherited state; receiver and
delegated worker writes belong to their own runs. Handoff memory is separate from
the original delegation text used by the command reviewer.

`history_read` retrieves text and tool evidence from the caller's original raw
history or an explicitly granted frozen handoff source. It does not accept an
arbitrary run ID, filesystem path, or permission inferred from review lineage.
Results have bounded pages and content chunks, stable record references, and
continuation cursors. System instructions, private reasoning, opaque compaction
payloads, and image binaries are excluded. Original history remains available
after compaction; this change does not implement a full context reset.
Pages contain at most four records, with each content chunk bounded to 4 KiB.
Frozen evidence is sanitized with the snapshot redactor before persistence;
handoffs retain at most eight explicit sources and 8 MiB of evidence. Exceeding
these bounds fails the handoff while leaving the source recoverable.

## Approval and recovery

Working notes, retrieved history, and summaries never create host approval
evidence. `auto_reviewer` continues to receive the original host user requests,
question answers, and their original reference context under the same policy.
Agent behavior can change the proposed command or justification, but the sources
that establish authorization do not change.

An unconsumed final assistant explanation has a durable reference ID independent
of notes. Restoring or handing off that ID preserves the original text and source
turn for terse responses such as “いいよ”. Saving the new host evidence and
consuming the reference share a transaction, so failed writes do not consume it
and an old snapshot cannot resurrect a consumed explanation.
A reference captured before storage was configured can be saved once when a
store is attached. The live reference permanently records that promotion;
missing or consumed durable references are never recreated from RAM.

Note writes acknowledge persistence before success. Context boundaries prepare
their selected note and source access before replacing the visible history.
Failed persistence must leave the previous context recoverable. Existing
interrupted-tool uncertainty notices remain in force; memory never authorizes
replaying a tool with an unknown outcome.

## References and evaluation

The original design drew on notes combined with compression and continuation.
[oh-my-openagent ultrawork](https://github.com/code-yeongyu/oh-my-openagent/blob/fe453f040807a12743a574bad43a2e06c2067159/packages/prompts-core/prompts/ultrawork/default.md#L220)
instructs the model to maintain and reread a notepad; these reads still depend on
model behavior. Codex's experimental
[history-notes tools](https://github.com/openai/codex/blob/3ee47c10513f1bb57d869ea650aba28638e3d11c/codex-rs/ext/history-notes/src/tools.rs)
and [new-context handler](https://github.com/openai/codex/blob/3ee47c10513f1bb57d869ea650aba28638e3d11c/codex-rs/core/src/tools/handlers/new_context_window.rs)
combine editable notes with original-history retrieval and a context boundary.
Their source availability is not a claim of universal feature availability or
measured benefit.

Offline regression tests check delivery, latest/clear semantics, source scope,
failure recovery, approval provenance, and provider cache reuse. They do not
establish improved model decisions or reduced token cost. A subsequent paired
evaluation should compare the old ledger, no notes, and current notes under the
same compaction conditions, measuring constraint adherence, repeated
investigation, evidence-backed reports, and total cached/uncached tokens. Full
context reset is a separate experimental variable.
