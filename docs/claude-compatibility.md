# Claude subscription compatibility

The OAuth adapter follows OMP's direct Messages transport strategy. API-key
requests continue to use the unmodified Anthropic wire format. Tool execution,
permissions, schemas, arguments, and stored tool names remain evorch's contracts.

The reference is OMP commit
[`470b7ef`](https://github.com/can1357/oh-my-pi/tree/470b7efb67673960d37e3e64584669e6e9caad1c)
and an isolated capture of Claude Code **2.1.322**
(`@anthropic-ai/claude-code-linux-x64`, native binary SHA-256
`bfb3703ad5380242328459bc62ada4bde8ae7ba57c6e1eeadb9c00e08874c478`, static build
time 2026-11-10T00:15:08Z). The current profile uses CLI **2.1.322**, SDK **0.128.0**
(unchanged since 2.1.295) and runtime **v26.12.1**. The OMP commit was not
re-analyzed for 2.1.322; OMP at the reference commit pins CLI 2.1.280 and
SDK 0.112.1, and these older constants were not copied. The package registry's
tarball integrity value and the extracted native binary's SHA-256 are different
digests; only the latter identifies the inspected executable.

### 2.1.322 capture conditions and evidence

Linux x86_64 only, with a dummy OAuth token and no real endpoint, account or
normal Claude configuration. The native binary ran in unprivileged user, network,
PID, IPC and UTS namespaces with a read-only Nix store, a synthetic home and
workspace, and a cleared environment. Before every launch a positive control
required that only `lo` existed with no routes, a loopback TCP connect succeeded, and
direct IPv4/IPv6 connections failed with `ENETUNREACH`; any failure stopped the run.
A CONNECT proxy accepted only `api.anthropic.com:443` and forwarded it, without DNS,
to a fixed loopback TLS listener whose dedicated CA was trusted through
`NODE_EXTRA_CA_CERTS` with certificate and hostname verification enabled.
Nine Messages requests were observed: an interactive main call (default model and
`claude-fable-5-2` with `xhigh`), print/SDK calls, a Read tool continuation
and a WebSearch main/auxiliary/main sequence. The settings/policy endpoints returned
synthetic `{}`, so remote feature flags, plan state and managed policy were not
reproduced: the absence of a beta in these requests does not show that a real
account never sends it. The raw requests, prompts, tool definitions and credentials
are not stored in the repository.

| Evidence | Findings |
| --- | --- |
| Native measurement | CLI/runtime/User-Agent, ClientHello, header casing/order, beta list, body field order, billing checksum and suffix, metadata and session header equality |
| Static analysis | Conditions of cache keepalive, thinking resumption and cache eviction betas; effort/display mapping |
| Not measured | Real endpoint acceptance, subscription eligibility, billing or usage-credit routing, actual cache hits, forced `tool_choice`, child/compaction requests, features that need real flags, other OS/architectures, TLS session resumption on a new connection |

## Compatibility boundary

| Surface | Subscription behavior |
| --- | --- |
| Authentication | Bearer token, `/messages?beta=true`, redirects disabled |
| Identity | CLI User-Agent and system identity; Stainless headers; persistent installation seed and account-scoped device ID; matching metadata/session header |
| Client tools | OMP `_` prefix; evorch-specific tools have an inner MCP-style namespace |
| Arguments and schemas | Original evorch definitions; no `path`/`file_path` translation |
| System | Billing control block, CLI identity, original instructions |
| Body | CLI field order; checksum patches the final serialized bytes |
| Caching | One-hour system/tool/message markers; original history retained |
| Thinking | Explicit model-validated adaptive/budget controls, summarized adaptive display, and keep-all context management |
| Compression | gzip, deflate, Brotli, and Zstandard accepted and decoded |
| Transport | OAuth-only BoringSSL client, captured TLS ClientHello, HTTP/1 header casing/order, no hidden retries |
| Correlation | Attempt UUIDs, request class, truthful prompt provenance, previous successful response IDs |

The beta profile contains the common inference defaults and extended-cache TTL.
Effort is advertised only when its field is sent; cache-diagnosis accompanies
the diagnostics envelope. Adaptive thinking requests `display: "summarized"`,
as native Claude Code does with `showThinkingSummaries` enabled, so the
transcript receives readable thinking. Native's default `updates` display omits
thinking text, and its `thinking-display-updates` beta is not sent. Optional OMP features such as
fallback credits, fast mode, deferred tools, server compaction, and tool-search
controls are not enabled by this adapter. No one-million-context beta is added.
The adapter keeps original tool definitions. It does not advertise deferred
tools, mid-conversation system changes, or thinking-binding controls that it
does not implement.

## Installation and request identity

Application composition and GUI reload share `claude-installation-id` in the
selected user configuration directory. The UUID seed is atomically published,
survives process restarts, and does not change with token rotation, profile
renaming, or endpoint changes. OMP's account-scoped SHA-256 derivation produces
the device ID. Invalid existing state fails without silently rotating it.
API-key clients never read or create this file. Library callers can inject the
seed explicitly; otherwise they share a process-local seed.

Provider session IDs follow [OMP's per-agent session mapping](https://github.com/can1357/oh-my-pi/blob/470b7efb67673960d37e3e64584669e6e9caad1c/packages/coding-agent/src/session/session-metadata.ts):
each independent evorch run has a stable session. The native CLI instead shares
its session with in-process subagents. Metadata and the session header always
carry the same ID; evorch retains its run-specific cache affinity.

The first Messages call binds the account from the same token bundle used for
authentication. Constructing a client before login or listing models does not
freeze a missing account. Same-account token rotation preserves identity. A
different account on a used client fails before refresh or inference HTTP and
requires a new client and conversation, preserving the warm input and signed
history contract. Correlation scopes also include the derived device identity.

Each HTTP attempt gets a new client request UUID. Accepted human inputs supply
a prompt UUID that stays fixed through tool calls and retries. Provenance and
indices are sent only when known; restored or internally generated inputs are
not relabeled as human requests. Response correlation is bounded and scoped to
session and purpose. Only a successful parsed response or completed SSE message
advances its `request-id` and `message.id`; failed or canceled attempts do not.
Tool durations are omitted because this adapter does not have measured values.
Request classes follow the inspected CLI's classification: agent requests use
`main` or `subagent`, compaction uses `compaction` even inside a child run, and
other purposes use `auxiliary`.
Known child runs send their actual run ID in `x-claude-code-agent-id`, including
compaction. `x-claude-code-parent-agent-id` is sent only when the parent is itself
a known subagent. Root requests omit both, and first-level children omit the parent
header. IDs retain printable ASCII while percent signs,
control characters and non-ASCII UTF-8 bytes use uppercase percent encoding.
Resuming a saved input preserves its prompt identity and counts while rebinding
ancestry to the current task. Agent type is omitted because this adapter has no
verified native agent catalogue mapping.

## TLS and HTTP reference

The subscription client uses pinned `wreq` 0.16.1 with a dedicated BoringSSL
profile. The reference Linux ClientHello (2.1.322) is 1,508 bytes. Its
`signature_algorithms` list is, in order: ECDSA P-256/SHA-256, RSA-PSS-RSAE
SHA-256, RSA PKCS#1 SHA-256, ECDSA P-384/SHA-384, RSA-PSS-RSAE SHA-384, RSA PKCS#1
SHA-384, RSA-PSS-RSAE SHA-512, RSA PKCS#1 SHA-512, Ed25519 (`0x0807`), ECDSA
P-521/SHA-512 (`0x0603`), RSA PKCS#1 SHA-1. The last two before SHA-1 are the only
difference from the 2.1.295 reference (1,504 bytes): with those two removed the
normalized handshake and the TLS record boundaries were identical. The fixture
was derived from the native capture, not from Rust output. The pinned BoringSSL
build accepts the names `ed25519` and `ecdsa_secp521r1_sha512` in its signature
algorithm list and emits them in that order. The advertised list only offers
algorithms; certificate and hostname verification are unchanged. Local comparison
excludes only the random value, session ID, and key-share public keys, then
compares every remaining byte. Header casing and order also follow the captured
CLI request. The generic API client continues to use its existing reqwest path.

Certificate and hostname checks stay enabled. `NODE_EXTRA_CA_CERTS` adds trusted
certificates only to the subscription client; malformed configuration fails
without exposing certificate contents or paths. Redirects and automatic retries
are disabled. A connection can be reused after a complete response, while
canceling an unfinished response drops its transport work.

This introduces a Rust 1.99 requirement and CMake, Perl, Git, and bindgen build
dependencies in Nix and CI. Linux packages retain the matching C++ runtime and
verify CLI startup and all shipped ELF dependencies without `LD_LIBRARY_PATH`;
the development shell and Nix test runner provide that runtime separately.
Reproducing a captured handshake does not prove
every platform, TLS resumption behavior, or future service-side classification.

## Checksum reference

Version suffixes select UTF-16 code units 4, 7, and 20 from the first wire user
message's first text block, substituting `0` when absent. SHA-256 of salt
`59cf53e54c78`, that sample, and the CLI version supplies the first three hex
digits. `Reply LOCAL_CAPTURE_OK.` produces `951` with version 2.1.322 (and `bef`
with 2.1.295), so a version update always changes the suffix. In a native request
the first wire user block can be a meta wrapper or reminder; the suffix sample is
the logical user text block, not necessarily the first block.

The native checksum (unchanged from 2.1.295 to 2.1.322) differs from OMP's older whole-body algorithm.
It uses XXHash64 seed `0x4d659218e32a3268` and the low twenty bits, excluding
literal model values and routing fields (`max_tokens`, `fallbacks`, and
`fallback_credit_token`) **from hashing only**. The transmitted fields are kept.
Only the five digits in the adapter's first billing block are replaced; sentinel
strings in user text, tool schemas, arguments, and historical results stay intact.

A local HTTPS capture of the installed native CLI produced a 63,019-byte
request with checksum `bf7d5`. Restoring its billing placeholder and applying
the native exclusions reproduced `bf7d5` with both Bun and Rust; OMP's older
whole-body calculation produced `728b0`. In the 2.1.322 capture all nine Messages requests reproduced
their checksum and suffix with a standalone BigInt XXH64 and a C XXH64 library
(xxHash 0.8.3 through Python ctypes); none used a parse/re-serialize oracle. Fixed independent synthetic vectors
for 2.1.295 and 2.1.322 are checked into the fingerprint tests. Fallback fields and
exotic inputs (lone surrogates, extreme values) were not exercised against 2.1.322. The captured CLI prompt and bundled
tool definitions are not copied into the repository.

## Cache and signed replay

Billing is a request transport control envelope. The provider binds signed
thinking to the stable shaped model input before adding that envelope. It never
rewrites already-sent history to insert a new checksum.

HTTP-boundary tests independently validate the billing suffix, typed correlation
fields, diagnostics, and final-byte checksum before separating those transport
controls from the model input. The
existing strict prefix assertions then compare instructions, settings, tools,
schemas, and every prior message. They also reject deliberate mutations with
valid recomputed checksums. The general Anthropic cache contract is unchanged.

These tests protect input preservation. Synthetic cached tokens do not establish
the service's cache hit rate, subscription eligibility, or usage-credit routing.
Those remain service-side decisions and require live account validation.

## Hosted web search

Both API-key and subscription clients expose Anthropic's
[server-side web search](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool)
through evorch's existing `web_search` tool. The selected model and provider
credentials are reused for a separate Messages exchange with
`web_search_20250305`; ordinary chat tools and history retain their existing
wire format. Server tools keep their official names and schemas, bypassing the
subscription adapter's client-tool namespace conversion.

Static inspection of the Linux x86_64 Claude Code 2.1.295 native binary
(SHA-256 `3886a7d7bcf6f40a587b9fd0f1b411797aa54e3a2c45f2d5c92d68d829bc6930`)
on 2026-10-17 confirms a separate WebSearch Messages call with that tool version,
at most eight searches, and prompt caching disabled. This is static evidence;
no live account was used for this feature's validation. The existing OAuth
identity, account binding, transport, and final-byte checksum are reused.

The search exchange uses JSON responses and automatic tool choice. This differs
from the inspected native forced choice and disabled thinking: the
[current tool-use documentation](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools)
restricts forced choice on some models. A completed, matching server search
result is required even if the model returns prose. Paused responses retain
their opaque content unchanged during bounded continuation. Returned sources
are deduplicated and bounded; encrypted search content stays inside the search
exchange. Each completed request's usage is recorded, including requests whose
search results fail validation.

Search does not install cache breakpoints or replace the chat cache diagnostic
baseline. API-key and OAuth runtime tests insert searches between ordinary
streamed tool turns and check both input-derived cache reuse and independent
wire-prefix preservation. Existing fallback behavior applies to rate limits,
server errors, timeouts, and explicit hosted-tool unavailability. Mock contracts
do not establish live subscription availability or billing eligibility.

Anthropic's [current SDK and third-party app guidance](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)
states that those clients can draw from subscription limits. Its
[usage-credit guidance](https://support.claude.com/en/articles/12429409-manage-usage-credits-for-paid-claude-plans)
describes paid overflow after included limits are reached and usage credits are
enabled. This compatibility adapter does not guarantee billing eligibility or
change the account's usage-credit settings.

## Intentional differences from native 2.1.322

The profile aligns the CLI/SDK/runtime identity and the TLS handshake; it does not
claim every native behavior. These differences were measured against 2.1.322 and
are deliberately kept, because each needs a stateful feature or a provenance
evorch does not have, and adding only the header or beta would risk rejection or
rewriting an already-sent prefix:

| Native 2.1.322 | evorch |
| --- | --- |
| Per-message `output_config.effort` in a `role: system` message (`per-turn-control`, `mid-conversation-*`, `tool-provenance`, `thinking-binding-controls` betas) | Top-level effort only; those betas are not sent |
| `cache-keepalive`, `thinking-resumption`, `prompt-caching-evict` betas (not sent in the nine observed requests; conditions are gated by flags/state) | Not implemented, not advertised |
| `x-claude-code-sandy-plover` (empty list, then the names of used tools) | Not sent; the header would need evorch's real wire tool names and scope |
| `thinking.display` `updates` (interactive) or omitted (print) with its beta | Adaptive thinking requests `summarized`; the beta is not sent |
| Default effort `medium` always explicit | No effort is sent unless a model/user setting asks for it |
| `metadata.user_id` JSON key order `device_id`, `account_uuid`, `session_id` | `device_id`, `session_id`, `account_uuid`; IDs and the session mapping are kept, never rotated for ordering |
| `max_tokens` 128000 on every model | Model catalog and user settings decide the limit |
| `cc_entrypoint`/turn origin/prompt index from the real UI | Provenance only when known; never relabeled as human input |
| WebSearch auxiliary request with automatic tool choice | Same; forced tool choice for other models was not measured |

The WebSearch auxiliary request in the 2.1.322 capture used
`web_search_20250305`, at most eight uses, automatic tool choice, no cache markers
and no thinking. The earlier note about a forced choice comes from static analysis
of 2.1.295 and was not re-measured here.

## Updating the profile

Use the repository's [Claude Code compatibility skill](../.agents/skills/claude-code-compatibility/SKILL.md)
for repeatable analysis and isolated captures. Review and improve the skill's
procedures and offline helper during each follow-up; keep current profile values
in this document and the implementation.

1. Pin the new CLI version, the extracted native binary's SHA-256, and the OMP
   commit; also record the evorch HEAD at the start and the end of the analysis, because
   concurrent merges can already cover a difference. Inspect an isolated installation;
   never use a real credential or the user's normal Claude configuration for
   captures. Direct a dummy token to a loopback mock. For the official-host
   branch, use a locally trusted TLS mock and a CONNECT proxy that rejects every
   destination except `api.anthropic.com:443` and forwards it only to loopback.
2. Compare CLI/SDK/runtime versions, headers, beta selection, body ordering,
   cache markers, identity, model controls, and raw checksums. Capture an ordinary
   interactive main request and a tool continuation. Distinguish auxiliary title
   requests and print/SDK mode from the CLI main path. Check child-agent request
   classification and the compaction override as separate cases.
3. Update `profile.rs` and the independently derived checksum vectors together.
   Compare send, fragmented SSE, interleaved tools, signed thinking, replay,
   token rotation, and client recreation; verify API-key requests remain intact.
4. Compare the normalized raw ClientHello and HTTP header order. Validate trusted
   and untrusted certificates, hostname mismatch, CONNECT proxy, pooling,
   cancellation, redirects, and connection failure without automatic retry.
5. Run provider tests, runtime subscription cache tests, and
   `scripts/check-cache-contracts.sh`. Preserve the general prefix checker and
   negative mutation cases. Complete Clippy and formatting checks before release.

The profile is immutable during a run. A server minimum-version rejection
reports that an update is needed, with a sanitized required version. evorch does
not adopt a new User-Agent/billing version midway through a warm conversation.
