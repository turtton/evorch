# Codex subscription compatibility

The `openai-codex` provider talks to the ChatGPT Codex backend
(`https://chatgpt.com/backend-api/codex`) through the Responses API over HTTP and
SSE. Tool execution, permissions, schemas, arguments, and stored tool names remain
evorch's contracts; only the wire shape follows the Codex CLI.

The reference is the Codex CLI source tag
[`rust-v0.166.0`](https://github.com/openai/codex/tree/rust-v0.166.0). The version
advertised on the wire is `providers::CODEX_CLIENT_VERSION` and must be moved
together with this reference. Paths below are relative to `codex-rs/` in that tag:

| Short | File |
| --- | --- |
| `client.rs` | `core/src/client.rs` (`build_responses_request`) |
| `common.rs` | `codex-api/src/common.rs` (`ResponsesApiRequest`) |
| `metadata.rs` | `core/src/responses_metadata.rs` |

The official OpenAI dialect (`type = "openai"`) shares the conversion code in
`crates/providers/src/wire/responses/` but does **not** follow Codex-specific rules;
every dialect-specific rule lives in `impl ResponsesDialect`.

## Compatibility boundary

### Headers

| Header | Behavior |
| --- | --- |
| `Authorization` | `Bearer <access token>`; matches |
| `chatgpt-account-id` | Sent when the token carries an account ID; matches |
| `originator` | Always `codex_cli_rs`; Codex TUI sends `codex-tui` (intentional, harmless) |
| `User-Agent` | `codex_cli_rs/<version>` only; Codex adds OS, architecture, and terminal (intentional) |
| `x-openai-internal-codex-responses-lite` | `true` on a Responses Lite request, including a 401 resend and the compaction call; absent otherwise; matches |
| `version` | The advertised client version; matches |
| `session-id`, `thread-id`, `x-client-request-id` | For a request that belongs to a run, all three are one value for the whole run: the UUIDv5 of the run ID in the OID namespace (`Uuid::new_v5(&Uuid::NAMESPACE_OID, run_id)`; run IDs are ULIDs, not UUIDs). It is identical in every ordinary turn, the compaction call, a 401 resend, hosted web search and image generation of that run. Codex sends its thread ID in `thread-id` / `x-client-request-id` and a session-derived value in `session-id`; a run is evorch's thread and session, so they coincide. A request without a run (no observation) keeps the older behavior: the client's own session UUID in `session-id` and a fresh UUID per HTTP request in the other two |
| `OpenAI-Beta` | Not sent on HTTP, on any path (responses, compaction, hosted web search, image generation); matches Codex. Only Codex's WebSocket handshake sends a beta value, and evorch has no WebSocket |
| `Accept`, `Content-Type` | `text/event-stream`, `application/json`; matches |
| `x-codex-*` routing, window, turn-state, and metadata headers | Not sent (difference) |
| `Content-Encoding` | Bodies are not compressed; Codex uses zstd (difference) |

### Request body

Field order follows `ResponsesApiRequest` in `common.rs`:
`model`, `stream`, `service_tier`, `input`, `tools`, `tool_choice`,
`parallel_tool_calls`, `reasoning`, `store`, `include`, `prompt_cache_key`, `text`.
evorch does not emit `stream_options` or `client_metadata`.

| Field | Behavior |
| --- | --- |
| `instructions` | **Absent.** Codex has no such field. Concatenated system messages (joined by a blank line) become one developer message at `input[0]`, as in `client.rs`; an empty system text adds no message |
| `input[0]` developer message | `{"type":"message","id":"msg_<uuid>","role":"developer","content":[{"type":"input_text","text":…}]}` |
| developer message ID | `msg_` plus a UUIDv5 of the instruction text under an evorch-fixed namespace. Codex derives the namespace from the thread ID; evorch's text-only ID is the same for equal text across runs and restarts, which keeps the input prefix byte-identical (intentional) |
| `input[0]` for Responses Lite | When the model is Lite and has tools: `{"type":"additional_tools","id":"at_<uuid>","role":"developer","tools":[{"type":"namespace","name":"functions","description":"","tools":[…]}]}`, ahead of the developer message, then the history. The ID is `at_` plus a UUIDv5 of the serialized tools under an evorch-fixed namespace (Codex derives the namespace from the thread ID), so equal tools keep one ID. No item without tools |
| `tools` | Sorted by name, always an array, empty included, for non-Lite models and for models the catalog does not describe. **Absent** for Responses Lite models (the tools are in `additional_tools`) |
| `tool_choice`, `store`, `stream` | `"auto"`, `false`, `true`; matches |
| `parallel_tool_calls` | `true`; `false` for Responses Lite models, as Codex |
| `reasoning` | `effort` is the explicit setting, else the model's `default_reasoning_level`, else `medium`. `summary: "auto"` is sent whenever the model accepts the parameter (`supports_reasoning_summary_parameter`, true when the catalog is silent). The catalog's `default_reasoning_summary` (`none` for the Lite models) is ignored on purpose, so evorch keeps requesting summaries (difference). `context: "all_turns"` for Lite models, omitted otherwise |
| `include` | `["reasoning.encrypted_content"]`, on the compaction request too; matches |
| `prompt_cache_key` | For a request that belongs to a run, the same run-derived UUIDv5 as `session-id` (Codex's default session-derived policy); unchanged within the run, so it is not a cache boundary. Omitted when there is no run. The key scopes the provider cache to a run: a delegated, forked or rewound run has its own run ID and therefore its own key, so it does not reuse its parent's cache (difference: Codex derives the key from the session, which a fork can inherit) |
| `text` | `verbosity` is the model's `default_verbosity` when the catalog says the model supports verbosity; `format` only for structured output, with evorch's schema name and strict flag (difference). No per-request verbosity setting exists yet |
| `service_tier` | Sent when requested (priority). When the model's catalog entry was fetched, it is sent only if the entry's `service_tiers` contains `priority`; a missing or empty list, other tiers, and a bare `additional_speed_tiers: ["fast"]` (the UI's fast support, not a wire advertisement) all omit it with a warning, as Codex checks the advertised tiers. With no fetched entry (no catalog, failed read, model not listed) it is sent as requested. The attempt observation records the tier actually sent |
| `max_output_tokens`, `temperature` | Never sent for Codex; matches |

### Model catalog and request shape

The model catalog (`GET {base}/models?client_version=0.166.0`, with the same
`version` header) decides the request shape per model. `CodexModelInfo.capabilities`
keeps what the shape needs: `use_responses_lite`, `default_reasoning_level`,
`supported_reasoning_levels`, `supports_reasoning_summary_parameter`,
`support_verbosity`, `default_verbosity`, and whether `priority` is advertised. Every
capability field is optional and malformed values (including malformed
`service_tiers` / `additional_speed_tiers` lists, which then advertise nothing) fall back
to their defaults, so an unfamiliar catalog never blocks requests. The catalog is read
from the same normalized base URL as `/responses` (`https://chatgpt.com/backend-api/codex`
for a bare host or a base ending in `/responses`). The usable context window is `context_window`
or, when that is missing or zero, `max_context_window`.

A `CodexClient` reads the catalog **once for its lifetime**, on first use by a
generation, a compaction or `list_models`, and fixes the result. A failed read (HTTP
error, timeout, invalid JSON) is fixed too: every later request uses the default
capabilities, which produce the same bytes as before the catalog was consulted, and a
single `ProviderCatalogFetch` warning diagnostic reports it. The shape therefore never
changes under an established prompt-cache prefix. A model missing from the catalog also
gets the default capabilities. Only a new client reads the
catalog again. `list_models` reports the stored outcome, failure included.

### SSE

| Event | Behavior |
| --- | --- |
| `response.output_text.delta`, `response.output_item.added/done`, function-call argument events | Handled by the shared Responses interpreter |
| `response.reasoning_summary_text.delta` | Handled as a single reasoning delta (no `summary_index`) |
| `response.completed` | `usage` is optional, as in Codex (`null` or an absent key); `response.id` is read for logs only (Codex requires it). See "Missing usage" below |
| `response.incomplete` | Mapped to a finish reason, not an error; `usage` is optional. See "`response.incomplete`" below |
| `response.failed`, top-level `error` | Flattened to evorch's error classes; Codex distinguishes more kinds and a nested `error` frame (difference) |
| Unknown events | Ignored; matches |
| Malformed JSON | An error (stricter than Codex) |

#### Missing usage

When the terminal event carries no usage, evorch finishes the response normally
and does not invent a zero: no usage event is recorded, no cache-reuse
observation is made, and one `ProviderUsageMissing` diagnostic (warning) is
emitted per response; the response id is logged. The response carries
`usage_reported = false` (`ChatResponse`, `CompactionResult`), and the
request-completed event carries the same flag with zero token counts as
placeholders. Downstream, those zeros are never treated as measurements:

- the usage ledger stores the flag (`usage_requests.usage_reported`, and
  `usage_daily.unreported_count` after rollup) and no cost for the request (a
  reported zero keeps `$0`); the Usage tab, in both recorded and current price
  modes, marks a total that contains such requests as a lower bound (`*`), and
  the evaluation report lists the models as "no reported usage" and treats the
  cost as incomplete, for ledger records and events alike;
- the GUI never prices the request as `$0`: the run's live cost is marked partial,
  the request is not shown as the latest context size, and the previous one is
  cleared;
- OTel spans omit `gen_ai.usage.*`;
- the runtime leaves `last_usage` empty, so context sizing uses the serialized
  byte estimate, and charges an estimate (the request as actually sent, with its
  instructions and tool definitions, plus the response) to the run budget, the
  goal usage, summary and official-compaction calls and the rules injection
  budget instead of zero;
- the next request's cache baseline is the last reported request.

The compaction request follows the same rule: its encrypted state is returned and
only the usage record is skipped (the budget is charged an estimate).

#### `response.incomplete`

| `incomplete_details.reason` | Codex 0.166.0 | evorch |
| --- | --- | --- |
| `max_output_tokens` | `ResponseIncomplete` error | Finish reason `Length`; partial output and tool calls kept (difference) |
| `content_filter` | `ContentFilter` error (retried) | Finish reason `ContentFilter`; partial output and tool calls kept, no retry (difference) |
| `interrupted` | Completed with `end_turn = false`, so the turn continues | Finish reason `Other("incomplete: interrupted")`; tool calls dropped, the run ends in an error, no continuation (difference) |
| other or missing | `ResponseIncomplete` error | Finish reason `Other("incomplete: <reason>")` (`Other("incomplete")` when missing); tool calls dropped, the run ends in an error |

Adding continuation for `interrupted` and a retry for `content_filter` is
deliberately out of scope for now.

### Transport and other surfaces

HTTP POST only (Codex prefers WebSocket and falls back to HTTP). OAuth refresh
uses the same endpoint, client ID, and JSON body as Codex; the refresh trigger
(ID-token expiry versus access-token expiry) and 401 recovery differ. Compaction
uses the normal `/responses` path with a trailing `compaction_trigger`, as Codex does.

## Caching

The developer message sits at the head of `input`, so it is part of the cached
prefix. Its ID and text depend only on the instruction text, hence a run's later
requests, requests after compaction, and a restarted run all begin with the same
bytes. Changing the instruction text changes the ID and invalidates the cache,
which is the same behavior as any other instruction change.

The cache contract (`mock_openai::cache_contract`) accepts a Codex request with or
without a top-level `instructions` string. It compares the developer message like
every other `input` item: unchanged across ordinary turns, and unchanged across a
verified compaction boundary.

For Responses Lite models the `additional_tools` item is the first `input` item and
the developer message the second, so both are part of the cached prefix. The tools are
the sorted, serialized tool set, so the item is byte-identical on every request while
the tool set is unchanged, across compaction boundaries and after a 401 resend. The
cache contract accepts `additional_tools` as an ordinary leading item (id prefix `at_`,
developer role, nonempty tool objects) and still rejects a change to it after the first
request.

The official-compaction call (`compaction_trigger`) is built like Codex's
(`compact_remote_v2_attempt.rs` sets the base instructions on the prompt and
`client.rs` puts them at the head of `input`): evorch passes the window's system
messages, the same ones an ordinary generation request carries, separately from the
history being compacted. The compaction request therefore has no `instructions` and
begins with the identical developer message (ID and text) followed by the compacted
history, with the same tools and settings, `include` and `prompt_cache_key` included.
Only the history is replaced, at the one verified boundary.

## Intentional differences

- Client identification (`originator`, `User-Agent`) is evorch's own.
- Equal developer instruction text gets the same message ID in every thread.
- `prompt_cache_key` and the identifier headers are derived per run, not per Codex
  session, so a fork or rewind starts a new key.
- Malformed SSE JSON is an error rather than being skipped.

- Reasoning summaries are requested for every model that accepts the parameter, even
  when the catalog's default summary is `none`.
- `additional_tools` and developer message IDs do not depend on the thread.

Other differences listed above are known gaps, not design choices.
