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
from committed event rows (assistant completed messages, user task prompts and
completed tool output; message parts are assembled in order). Deltas are ignored.
Other read models are compared with their last published DTOs. Host bus **and**
approval notification lag invalidate the epoch and terminate all old streams;
there is no silent lag recovery. Deletion or question routing scope removal also
invalidates old snapshots/replay/queued DTOs. Query reads refresh first, so a
persisted tombstone is honored even before its bus notification is processed.

Default bounds: replay 4096 events / 8 MiB, each subscriber 256 / 1 MiB, at most
1024 registered subscribers. A reserved terminal slot replaces an overflowing
queue with ResyncRequired(SlowSubscriber), then the stream ends. Shutdown and
unsubscribe similarly deliver a final SubscriptionClosed. Snapshot storage holds
four snapshots of at most 8 MiB each; continuation uses snapshot_page, not a new
live query. Larger snapshots fail Busy. Pages contain at most 200 items and
768 KiB of encoded item data (headroom below the protocol's 1 MiB frame).
Opaque server-side page cursors bind query + revision; live collection changes
require restarting pagination. History cursors instead freeze their durable event
high-water, allowing later committed messages without invalidating that history.

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
