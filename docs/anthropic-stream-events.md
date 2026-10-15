# Anthropic Messages stream contract

Audited on 2026-10-15 against the [streaming specification](https://platform.claude.com/docs/en/build-with-claude/streaming),
[thinking specification](https://platform.claude.com/docs/en/build-with-claude/thinking),
[preserved-thinking specification](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking),
and the official Python SDK at revision
[`2a53eaa6966b04560b6a72c28d3ace4389a9ac58`](https://github.com/anthropics/anthropic-sdk-python/tree/2a53eaa6966b04560b6a72c28d3ace4389a9ac58/src/anthropic/types).

## Event inventory

The stable and beta stream unions contain `message_start`,
`content_block_start`, `content_block_delta`, `content_block_stop`,
`message_delta`, and `message_stop`. The streaming specification additionally
defines `ping` and `error`.

| Content delta | Payload | Meaning |
| --- | --- | --- |
| `text_delta` | `text` | Append text to the indexed block. |
| `thinking_delta` | `thinking` | Append thinking to the indexed block. |
| `signature_delta` | `signature` | Set the indexed thinking block's opaque signature. |
| `input_json_delta` | `partial_json` | Assemble the indexed tool's JSON input. Only client `tool_use` becomes a local tool call. |
| `citations_delta` | `citation` | Append a citation to the indexed text block. |
| `compaction_delta` (beta) | optional `content`, `encrypted_content` | Replace the indexed compaction fields, including opaque replay metadata. |

The SDK [stream accumulator](https://github.com/anthropics/anthropic-sdk-python/blob/2a53eaa6966b04560b6a72c28d3ace4389a9ac58/src/anthropic/lib/streaming/_messages.py)
assigns each signature delta to `signature`; it does not concatenate signatures.
The beta accumulator similarly replaces `content` and `encrypted_content` on
each compaction delta, including null values.
Usage fields on `message_delta` are cumulative. An absent optional usage field
must preserve its prior value.

The stable content-start union contains:

- `text`, `thinking`, `redacted_thinking`, `tool_use`, `server_tool_use`
- `web_search_tool_result`, `web_fetch_tool_result`, `tool_search_tool_result`
- `code_execution_tool_result`, `bash_code_execution_tool_result`,
  `text_editor_code_execution_tool_result`, `container_upload`

The beta union additionally contains `advisor_tool_result`, `mcp_tool_use`,
`mcp_tool_result`, `mcp_tool_listing`, `compaction`, and `fallback`.
These are response shapes, not instructions to enable or execute these features
locally. A provider compaction block alone does not authorize replacement of
evorch's history.

## Reception and replay

New event, block, and delta types are permitted by Anthropic's versioning
contract. An unknown string discriminator must not by itself fail a run.
Known malformed payloads, malformed JSON, actual API `error` events, and
streams that never complete remain failures.

The Anthropic interpreter assembles content by block index instead of relying
on the generic accumulator's reasoning/text/tool regrouping. Display deltas
remain available, while the completed message retains opaque Anthropic blocks
for replay. Server-side tool inputs never become executable local tool calls.

Signed thinking includes empty `thinking` strings. Each signature belongs to
its original block; multiple thinking blocks and `redacted_thinking.data` must
survive persistence and replay in their original order. Opaque fields must not
be interpreted as ordinary display text or sent to another provider protocol.
Unsigned reasoning from other providers must not be synthesized into signed
Anthropic thinking.

Thinking signatures are bound to the preceding `system`, `tools`, and
conversation. Cache-control markers and generation settings are outside that
binding. Normal turns preserve the input prefix. A history-replacement boundary
such as local compaction invalidates signatures generated against the previous
prefix; signatures generated against the new prefix must then remain reusable.

Persistence must also preserve the credential-redaction contract. Opaque
signature and redacted-thinking bytes are not rewritten. Plaintext thinking and
tool payloads are still inspected for secrets. A signed payload requiring
redaction must not be saved as a usable, modified signed block.

The offline cache gate includes HTTP replay contracts and the runtime's
input-derived cache oracle, with independent assertions over captured wire
prefixes. No live API credentials or paid model calls are required.
