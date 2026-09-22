# ADR 0027 revision: explicit renewal of a team coordinator

Status: implementation proposal, pending canonical intent update in the host repository.

The user requested reconsideration of ADR 0027's blanket rejection of DynamicTeam
restoration. This revision preserves ADR 0026's existing run/task substrate and
separates **reusing conversation history** from **reviving execution authority**.
It does not resume old tools, processes, workers, or leases.

## Allowed expansion

`continue_goal` and `delegate_chat` may reuse a saved **root Orchestrator** history
that previously used DynamicTeam when all of these checks pass:

1. The snapshot identifies the coordinator run and the durable team ID. It stores
   no database path, writer handle, lease, ownership permit, or semaphore.
2. The caller supplies current DynamicTeam configuration, a nonempty delegation
   value, and a currently authorized TeamStore for that same team ID. A saved
   descriptor is not authority to open a database or grant capabilities.
3. The previous coordinator and all of its known descendants are stopped, including
   children still waiting for provider admission. Registration and this check share
   the admission/run lock order, so a child cannot disappear between the two stores.
4. The latest board in the caller-provided store can be validated and contains no
   claimed tasks. Expired claims are not automatically treated as safe by this
   restore path. Reconciliation must establish the prior owner's effects first.
5. No incomplete or cancelled tool execution with potential side effects remains
   in the run checkpoint.

`continue_goal` also accepts a persisted root after process restart, when no live
RunEntry exists, and verifies its root identity and role before registering it.
It retains the existing goal-root run ID; chat continuation allocates a new run ID.

The existing team initialization path reloads the durable board with its revision
and task generations. Completed work remains completed. Ready tasks remain ready;
restoration does not schedule them. Writer revision conflicts still fence stale
in-memory boards. Current caller authority supplies ownership, network access,
workspace settings, model choice, skills, and worker limit. No such value is
implicitly taken from a former live RunEntry.

For root histories (Single or DynamicTeam), old `memory` and `finding_store` configuration also
ceases to be a blanket blocker: the old MemoryBoundary and storage path are not
restored. Old lessons already present in conversation remain historical reference
text; a new boundary/store, if needed, is explicitly supplied by the current
caller. This is necessary for ordinary GUI-created goals, which set both fields. For Single
roots this uses a distinct caller-renewal marker; a team authority is not required.

## Boundaries retained

- `restore_and_deliver` is disk-authoritative and still rejects these records.
  Sending a message does not supply new team/storage authority.
- Team workers (`team_task`), non-root team runs, `learning_internal`, and
  `workspace_branch` remain unsupported. Restoring an old worker lease or branch
  checkout would require a separate fencing/reconciliation contract.
- Memory/finding configuration on child runs remains blocked. The existing
  ownership-only renewal rule is unchanged.
- A root's persisted TeamStore identity cannot, by itself, enable a GUI button to
  reconstruct authority. GUI callers must use their current project/thread setup.
- A restore refusal does not consume the last checkpoint. Ordinary snapshots
  still follow the existing synchronous consumed-marker rule at the existing
  entrances; this change introduces no alternate execution engine.

## Tool interruption and diagnosis

Before dispatching a tool batch, the runtime persists the last protocol-complete
history prefix and the batch's call IDs/names. The argument bodies are excluded
from this metadata. Failure to persist this intent prevents tool dispatch when a
run store is configured. Normal completion replaces this marker with the complete
history. Shell jobs whose terminal result has not been observed remain blocked
even after their start call returned a job ID or a terminal cleanup killed them. A failed later write leaves the prior checkpoint/intent intact.

A crash before a complete batch result, or a cancellation result that cannot prove
absence of partial effects, leaves `interrupted_tool_calls`. Every restore entry
rejects that history for execution when the registered permissions allow writes,
processes or network activity (unknown operations are conservative), even if a record flag was accidentally enabled.
The operator can inspect actual files, process state and durable task artifacts,
then start a **new run** from the existing durable-task continuation flow after
reconciliation. This patch adds no automatic acknowledgement or blind retry.

`AgentRuntime::restore_diagnostics` is read-only and reports the last successful
checkpoint timestamp and phase, history size, compaction checkpoint count, disk
restore support, conditional caller-renewal support, refusal reason, durable task
ID, team identity, and uncertain call IDs/names/result-observed and side-effect flags.
Incomplete read-only tools remain visible diagnostically but do not block history
reuse; they are never automatically replayed. Conditional
history support is not a claim that current authority or claim reconciliation has
already succeeded. A missing saved context is distinguishable from a corrupt one.

## Verification

Regression coverage includes restart with a completed team board and a new run
ID; missing/mismatched caller authority; live unclaimed descendants and pending provider admissions; persisted
claims; disk-authoritative message delivery; disagreement between record and
descriptor flags; durable tool intent before effects; refusal to repeat a tool
after restart/cancellation; and storage failure before dispatch.
