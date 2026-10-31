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
| `version`, `session-id`, `thread-id`, `x-client-request-id` | Present; `thread-id` / request ID are per HTTP request rather than per conversation (difference) |
| `OpenAI-Beta` | `responses=experimental` is still sent on HTTP; Codex does not send it (difference) |
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
| `tools` | Sorted by name, always an array, empty included; matches for non-Lite models |
| `tool_choice`, `store`, `stream` | `"auto"`, `false`, `true`; matches |
| `parallel_tool_calls` | Always `true`; Codex clears it for Responses Lite models (difference) |
| `reasoning` | `effort` defaults to `medium`, `summary` is `auto`; model capability is not consulted (difference) |
| `include` | `["reasoning.encrypted_content"]`; cleared on the compaction request (difference) |
| `prompt_cache_key` | Omitted for Codex (Codex permits omitting it) |
| `text` | Only for structured output; evorch's schema name and strict flag (difference) |
| `service_tier` | Sent when requested (priority) |
| `max_output_tokens`, `temperature` | Never sent for Codex; matches |

Responses Lite models (`use_responses_lite`) need a different request shape
(`additional_tools` in input, no top-level `tools`) and are not supported yet.

### SSE

| Event | Behavior |
| --- | --- |
| `response.output_text.delta`, `response.output_item.added/done`, function-call argument events | Handled by the shared Responses interpreter |
| `response.reasoning_summary_text.delta` | Handled as a single reasoning delta (no `summary_index`) |
| `response.completed` | Usage is required (Codex treats it as optional) |
| `response.incomplete`, `response.failed`, top-level `error` | Flattened to evorch's error classes; Codex distinguishes more kinds and a nested `error` frame (difference) |
| Unknown events | Ignored; matches |
| Malformed JSON | An error (stricter than Codex) |

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
verified compaction boundary. The official-compaction call replays history without
system messages and therefore has no developer message; it is not part of the
generation prefix.

## Intentional differences

- Client identification (`originator`, `User-Agent`) is evorch's own.
- Equal developer instruction text gets the same message ID in every thread.
- `prompt_cache_key` is omitted.
- Malformed SSE JSON is an error rather than being skipped.

Other differences listed above are known gaps, not design choices.
