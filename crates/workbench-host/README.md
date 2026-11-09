# workbench-host read side (ADR 0045 P3)

CommandHost::handle_query serves every protocol Query. Registered ProjectDirectory
entries define the visible project set (StaticProjects supports enumeration).
GetCoreStatus is deliberately minimal; activity is derived from visible runs.
Saved context metadata and deferred event readers are used, not eager context/body
hydration. GUI and HTTP/SSE transport integration are separate work.

## Snapshot and stream

CommandHost::projection() exposes subscribe_with_snapshot, snapshot_page and
unsubscribe. The projection mutex covers refresh, high-water selection, frozen
snapshot creation, replay selection and subscriber registration. Updates arriving
during that boundary are projected once after it, with a greater cursor. Replay
uses the same cursor/envelope payload as original delivery, with a new subscription
ID. IDs increase throughout the host lifetime, never reused across connections.
A resumed stream advertises its accepted after cursor as high_water. An expired,
future or foreign-epoch cursor yields a new snapshot and terminal ResyncRequired;
control events also advance sequence so clients cannot discard them as duplicates.

The core bus and the singleton sandbox approval registry wake the projection.
StorageHandle's weak commit observer wakes it after append_event / fenced append
success, independently of bus-vs-writer scheduling. MessageCommitted is read only
from committed event rows and Delivered human receipts (assistant completions/finish,
user task prompts/followups and tool output; message parts assemble in order). Deltas are ignored.
Other read models are compared with their last published DTOs. Host bus **and**
approval notification lag invalidate the epoch and terminate all old streams;
there is no silent lag recovery. Deletion or question routing scope removal also
invalidates old snapshots/replay/queued DTOs. Query reads refresh first, so a
persisted tombstone is honored even before its bus notification is processed.

Default bounds: replay 4096 events / 8 MiB, each subscriber 256 / 1 MiB, at most
1024 registered subscribers. A reserved terminal slot replaces an overflowing
queue with ResyncRequired(SlowSubscriber), then the stream ends. Shutdown and
unsubscribe similarly deliver a final SubscriptionClosed. Snapshot storage holds
four snapshots of at most 8 MiB each for five minutes; continuation uses
ReadSnapshot (or snapshot_page), not a fresh live query. Larger snapshots fail
Busy. Pages contain at most 200 items and
768 KiB of encoded item data (headroom below the protocol's 1 MiB frame).
Opaque server-side page cursors bind query + revision; live collection changes
require restarting pagination. History cursors instead freeze their durable event
high-water, allowing later appended event messages without invalidating that history.
Receipt or retention changes to the frozen prefix revoke the cursor with Conflict.

## Bodies, approvals and questions

Small committed content is inline; large inline bodies and stored blob tokens are
represented by host-local opaque BodyIds. Body references are bounded to 4096
entries and 8 MiB retained inline text. ReadBody loads only the requested source,
checks tombstones, masks secrets before slicing and returns UTF-8-safe chunks of
4..65536 bytes. Expired handles return NotFound; restart the history query. No
storage path or content hash becomes a public body identifier.

ListApprovals enumerates registry pending pages and retains observed terminal
decisions. It never substitutes summary text for an authorization decision.
CommandHost::full_approval_input requires the original ID, registry epoch and
revision, human provenance and the same thread ownership gate as the decision.
DecideToolApproval uses that registry's resolve CAS; first decision wins. Ingress
supplies actor and surface, not the browser payload. Projection epoch invalidation
does not rewrite registry approval IDs or their authoritative epoch.

Questions use ADR 0044's hydrated requester/routing/management records and surface
User or Lead targets. Lead scopes receive routed question events too. Same-host
answers retain ingress provenance. A legacy/externally saved host answer whose
surface is unknown has answered_by omitted, not a fabricated Desktop actor; known
agent origins retain their run identity. Answer-request receipts and provenance
are hidden after the immutable requester run/thread is tombstoned. No migration
or runtime execution/prompt changes are introduced.

## Committed transcript sources

ReadCommittedHistory and MessageCommitted share one canonical deferred scanner.
It emits completed/assembled assistant messages, nonempty finish results (unless
identical to that run's last assistant message), task prompts, delivered human
input receipts and tool outputs. Initial instructions are linked to the earliest
accepted human receipt ID in each run by acceptance_seq (all statuses): only a
Delivered initial receipt preceding that run's first task prompt suppresses the
prompt. Repeated descriptors of that initial task are deduplicated; changed
later instructions and distinct submissions of identical text remain visible.
Human inputs merge by acceptance timestamp and ID ahead of the corresponding stored event, matching GUI history replay.
Accepted, withdrawn, dropped and unknown receipts are not themselves committed
messages; a Delivered receipt is sufficient without persisting its notification.
The storage descriptor API leaves texts over 16 KiB in SQLite for ReadBody,
using SQLite's metadata-only octet_length for the byte count. Query paging and
projection refresh never resolve deferred bodies, including initial inputs.
Finish deduplication compares available segment digests/ref descriptors only.
Like GUI BodyText segment equality, multipart refs and a single whole-body ref
are incomparable and remain separate messages, without loading either body.
Missing/corrupt page-external blobs therefore cannot prevent reading an earlier
page. The feature-gated storage read meter tests zero full-body calls/bytes on
both history pages and subscription refresh, with an explicit ReadBody positive
control.

A late delivery or retention can change an already published message prefix.
The host verifies each run's previous prefix digest, advances the stream epoch
and requests resync rather than silently renumbering indices. Normal append-only
messages retain their indices. To detect these insertions, the current projection
rescans deferred event descriptors and committed receipt headers on refresh; it
retains only per-run counts and prefix digests, not historical full bodies.

The protocol currently lacks structured transcript variants for compaction
cards, tool call/status cards, artifact cards, reasoning and peer-addressed agent
messages. These are not disguised as human/assistant dialogue. Compaction and
agent-message transcript projection, including sender/recipient and checkpoint
metadata, remains a follow-up; run/delegation phase metadata still uses RunChanged.

## HTTP-friendly query additions

- `GetApprovalInput` / `QueryReply::ApprovalInput`: read complete canonical JSON
  (or the empty string for no input), secret-masked **before** UTF-8 slicing.
  Every request binds `approval_id + core_epoch + approval_revision` and checks
  the singleton registry again, using the same tombstone/ownership gate as the
  desktop helper. Inputs are at most 1 MiB and chunks 4..=65,536 bytes; offset 0
  starts a read, `next_offset` continues, EOF returns empty text/no next offset.
  A wrong epoch/revision returns `StaleEpoch` / `StaleRevision`; approved,
  denied, expired or cancelled entries return `AlreadyResolved`. No input-body
  capability survives resolution. As with other host queries, the transport is
  responsible for authenticating its human reader before calling handle_query.
  `ApprovalView.summary_truncated` describes registry/projection bounding;
  `input_bytes` counts original serialized input bytes before redaction,
  whereas the chunk's `total_bytes` describes the secret-masked representation.
  If masking expands that representation past 1 MiB, return LimitExceeded
  rather than silently truncating the complete input.
- `ReadSnapshot` / `QueryReply::Snapshot`: `snapshot_id + page` addresses
  the frozen cut after a subscription/connection closes. Every reply returns the
  same ID and original `high_water`, not the current live cursor. Continue with
  `page.next_cursor`, buffering later SSE events until all pages are applied.
  Retention is at most four snapshots (8 MiB each), with a five-minute absolute
  TTL, lazily pruned on snapshot creation/read. Reading does not renew the TTL.
  Count eviction, age expiry, epoch invalidation and revoked/mismatched cursors
  return `ResyncRequired`; acquire a new snapshot rather than mixing cuts.
- `ReadCommittedHistory.direction` defaults to `forward` when omitted.
  `backward` without a cursor reads the latest page; its next cursor reads the
  preceding page. Each page remains chronological (oldest first), ready to
  prepend unchanged. Cursors bind the direction, thread/run membership, frozen
  committed event cut and retained deferred descriptors. Neither direction
  resolves deferred bodies before returning pages; only explicit ReadBody does.
- Entity `Revision` values are **equality tokens**, not counters. Project,
  thread, run and receipt revisions may be content hashes: a smaller value is
  not an older state. Order updates using `EventCursor.core_epoch + sequence`.

Payload examples (wire u64 values are decimal strings):

```json
{"type":"get_approval_input","payload":{"approval_id":"approval-1","core_epoch":"7","approval_revision":"1","offset":"0","max_bytes":65536}}
{"type":"approval_input","payload":{"approval_id":"approval-1","core_epoch":"7","approval_revision":"1","offset":"0","text":"null","total_bytes":"4"}}
{"type":"read_snapshot","payload":{"snapshot_id":"snapshot-1","page":{"cursor":"opaque-next","limit":50}}}
{"type":"snapshot","payload":{"snapshot_id":"snapshot-1","high_water":{"core_epoch":"7","sequence":"40"},"page":{"items":[],"snapshot_revision":"40"}}}
{"type":"read_committed_history","payload":{"thread":"thread-1","direction":"backward","page":{"limit":25}}}
{"id":"approval-1","core_epoch":"7","revision":"1","thread":"thread-1","run":"run-1","tool_name":"shell","summary":"null","summary_truncated":false,"input_bytes":"4","state":"pending"}
```

## PR08-1: conversation bootstrap

- Negotiate `conversation.v1`. The single new top-level command is
  `Conversation`; its operations resolve an already registered absolute project
  root, create an execution with client-generated thread/submission IDs, prepare
  a legacy thread, upload an image, and discard an unused image. Project lookup
  never adds settings or asserts trust. Existing `SendChat` / `ContinueChat`
  remain the common continuation/queue path.
- `PrepareExistingThread` is deliberately narrower than general recovery: the
  persisted project root must match a currently approved host sidebar declaration,
  the old owner must be Released **and** its IPC endpoint demonstrably offline,
  and canonical ADR0024 claim must pass. Live/uncertain/stale owners are not
  forcibly acquired. Tombstones are checked before and after claim. Preparation
  binds the saved root; actual resume still uses the runtime's canonical restore
  through SendChat/ContinueChat. This is not PR05 checkpoint recovery.
- Uploads accept base64 bytes, never paths. PNG/JPEG/GIF/WebP must decode, match
  their MIME, fit 4096x4096 / 64 MiB decode limits and 512 KiB base64 per upload.
  Opaque IDs are scoped to project/thread and the core epoch. The bounded pending
  table holds at most 32 images / 8 MiB base64; explicitly discard unused IDs.
  Sent images enter the existing durable runtime input path; clients retain
  unsent bytes and re-upload after an epoch change. No streaming body is added.
- `Delegation::Team { value }` has the desktop team-objective meaning;
  `Reference` remains distinct and unsupported rather than silently starting a team.
- Keep one ClientConfig across reconnects: it preserves both request numbering
  and the logical receipt session. OutcomeUnknown requires GetCommandReceipt;
  Unknown is not proof that a command never ran (bounded receipts/core restart),
  so do not automatically resend. No disconnect means StopRun.
- PR08-2 wires the GUI adapter/cache; PR08s may namespace the existing top-level
  protocol. PR09–16 still own settings and other management surfaces; PR17 should
  reuse these bootstrap/attachment operations rather than reading client files.
