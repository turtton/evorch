# Opt-in core-client (ADR 0045 PR08)

Run `evorch-gui --core-client [--config-dir /absolute/config]` beside the matching `evorch-core` binary. Configuration identity uses `--config-dir`, then `EVORCH_CONFIG_DIR`, then the standard user configuration directory. Legacy embedded mode remains the default. The opt-in branch runs before legacy runtime/storage/ownership initialization and never falls back on connection, launch or protocol errors.

The client-only window supports the host's projects and threads, paged committed history, lazy committed bodies (no token streaming), Worker/Orchestrator chat, queued follow-ups/withdrawal/next-turn delivery, Team objectives, continue/stop, routed questions and complete-input approval confirmation. New conversations use client-generated thread/submission IDs; host ingress determines Desktop/Human provenance. Images are locally read (384 KiB maximum) and sent as bounded MIME/base64 bytes to obtain validated, epoch/project/thread-scoped attachment IDs. No local path is sent to or opened by the host. Team-objective image inputs are explicitly Unsupported.

The snapshot is staged separately while a bounded 256-event/2 MiB buffer collects concurrent events. Only events after its high-water are applied; overflow, epoch changes and terminal subscription signals request a new cut. Revisions are equality tokens, not sort keys. Epoch replacement discards the old projection and invalidates attachment IDs, but retains text/image drafts and unknown-command state. Images can be explicitly revalidated from retained bytes.

A lost command reply remains OutcomeUnknown. Reconnection uses the original receipt session and asks GetCommandReceipt; there is no unconditional retry. Unknown remains visible and retains its draft. Closing the window gives the client actor up to 500 ms to best-effort discard known unused attachment IDs, then aborts the actor/transport. It never sends StopRun/StopChat and does not own the core process. Reconnect is explicit after connection failure.

The optional existing-thread import requires its canonical project already approved by the current host. The host rechecks tombstones and project identity/trust, and uses ADR0024 claim/restore only for a Released, demonstrably offline foreign owner. Live and uncertain owners are not acquired. The window never reads the owners registry, run DB or legacy sidebar itself.

Unmigrated functionality is labelled Unsupported: project registration/trust changes, settings/model selection, lane/PR services, fork/rewind/delete/archive, files, terminals and browser. These must migrate through PR09–16 rather than direct DB access. The main embedded window and its existing tests remain separate.

## Handoffs
- PR08s: move conversation bootstrap/image operations into the planned protocol namespace. PR08 added only one top-level Command variant, Conversation, with nested operations.
- PR09–16: replace the explicit Unsupported affordances with protocol capabilities; do not import an embedded runtime into this branch.
- PR17: reuse ResolveProject/CreateExecution/CreateGoal/PrepareExistingThread and validated IDs. Keep receipt_session across reconnects; never interpret Unknown as permission to resend. Team carries the full delegation objective, not a named Reference.
- Attachment limits currently bound individual uploads and an epoch-local pending table. Larger transfers, durable attachment retention and streaming are not part of PR08.

## Upload lifecycle and recovery

Enqueue synchronously registers a stable local draft ID, before the connection actor runs. Queued/in-flight/OutcomeUnknown images block send and discard; a full/closed queue keeps their bytes as unsent drafts for explicit retry. Only a known rejection or unsent failure makes an image retryable. UploadImage lost replies retain the original request/session and draft ID. Applied ImageUploaded receipts update that draft; Unknown remains blocked across snapshot/epoch replacement, with no replay or reallocation. Explicit reconnect performs receipt reconciliation again, including when the previous receipt was Unknown.

Crash cleanup follow-up (not implemented in PR08): expire unused host attachments after a five-minute absolute TTL, lazily pruning on upload/read and never extending retention merely by reading. Reject expired IDs and require explicit revalidation from retained local bytes. Currently a crash or interrupted shutdown can leave entries until the host epoch ends; the existing 32-entry / 8 MiB bound limits growth, but is not time-based reclamation.
