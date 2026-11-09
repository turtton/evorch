# External MCP tools client: specification fixtures (T0)

These are manually authored wire expectations, **not** generated from product
request builders. T0 provides contracts and an inert scaffold, not a completed
external MCP client. Product conformance tests belong to C/D and runtime E/G.

## Verified references

Fetched from the public specification on 2026-11-06 (UTC). Version: **2026-07-28**.
The normative schema is linked by [the specification index](https://modelcontextprotocol.io/specification/2026-07-28/).

- [Versioning and Compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)
  — per-request version declarations, supported/requested negotiation, era probe.
- [Basic metadata](https://modelcontextprotocol.io/specification/2026-07-28/basic/index#meta)
  — namespaced request metadata (spelled out below).
- [stdio](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio)
  — Sending/Receiving Messages, Request Metadata, Cancellation, Shutdown,
  Backward Compatibility.
- [Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
  — Sending Messages, Request Metadata, Custom Headers from Tool Parameters,
  Value Encoding, Backward Compatibility.
- [Tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)
  — listing/calling tools, x-mcp-header validation, content and error results.
- [MRTR](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr)
  — InputRequiredResult (requestState is opaque; this client does not continue).
- [Cancellation](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/cancellation)
  — transport-specific cancellation and in-flight ID/race rules. The planned
  /basic/utilities/cancellation URL redirects here; this is a URL relocation,
  not a change to the plan's cancellation policy.
- Legacy **2025-11-25**:
  [cancellation](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation),
  [lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
  [transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports).

Modern request params._meta contains the literal namespaced keys
io.modelcontextprotocol/protocolVersion, io.modelcontextprotocol/clientCapabilities,
and io.modelcontextprotocol/clientInfo. Initial capabilities are {}. The plan's
short names describe concepts, not unprefixed wire keys. Modern does not use
initialize, ping, sessions, GET SSE or Last-Event-ID. Legacy uses initialize then
notifications/initialized; missing legacy resultType means complete.

## Fixture contracts

| Fixture | Normative behavior represented | Product test owner |
| --- | --- | --- |
| modern-stdio.jsonl | SHOULD probe server/discover; MUST include version metadata, newline JSON-RPC and modern resultType; list/call use original remote name | D/C |
| legacy-stdio.jsonl | SHOULD fall back on any non-modern error (not just -32601); MUST initialize, validate version then notify initialized; absent resultType accepted | D/C |
| modern-http.json | MUST POST with both Accept types, required metadata headers matching body; MUST mirror valid x-mcp-header values; SSE completes on final correlated result while peer is still open | C |
| legacy-http.json | 400 non-modern body permits legacy fallback; session is retained only in this era and reused with protocol header; notification returns 202 | C |
| negotiation-errors.json | -32022 carries supported/requested and HTTP 400; recognized modern error MUST NOT cause initialize fallback; 401 is not fallback; unsupported MRTR returns a dedicated error without replay | C |
| cancellation.json | modern stdio MUST notify in-flight issued IDs; modern HTTP closes stream (no notification); legacy initialize MUST NOT be cancelled; completed/unissued IDs never notified; legacy HTTP best-effort retains session | C/D |

The modern HTTP transcript includes discovery/list/call; its list advertises the
region parameter annotation that the call mirrors. C adds malformed annotation
and schema-isolation cases. Cancellation expected values are compared to the
pure shared policy in T0; actual side-channel I/O/flush/reap remains C/D coverage.

The client is an **MCP tools client**, not a complete implementation of every
capability. OAuth, MRTR continuation, resources/prompts exposure, subscriptions /
listen and 2024 HTTP+SSE are out of scope. Modern stdio restart is a SHOULD; we
restart at most once but deliberately do not replay a call whose outcome is
unknown, because duplicate side effects are unsafe. TTL never mutates a frozen
conversation's tool set, and list caches are not shared between independent runs.

## Driver

- shared-test-support/mcp/mod.rs (relative to the tools crate) loads JSONL or a
  JSON {steps:[...]} document at **run time**. Events are send, receive, signal,
  gate and close. send checks an actual client value; receive supplies a peer
  value. The mpsc driver waits for exact named control events, never sleeps.
- modern-http's gate client_completed_before_eof lets the test prove completion
  without peer EOF. The final_response_written signal is explicit synchronization.
- mcp-test-server is a local Rust fixture binary accepting a JSONL path. It checks
  stdin frames and flushes stdout frames independently of the product builders.
  D owns extensions for lifecycle/control-pipe scenarios. It needs no npm/service.
- Read CARGO_MANIFEST_DIR and CARGO_BIN_EXE_mcp-test-server at run time. Do not
  include_str! files from tests/ into a library. No fixture contains real secrets.

See [CONTRACTS.md](CONTRACTS.md), [OWNERSHIP.md](OWNERSHIP.md), and
[TEST-MAP.md](TEST-MAP.md) before parallel implementation.
