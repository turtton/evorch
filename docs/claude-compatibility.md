# Claude subscription compatibility

The OAuth adapter follows OMP's direct Messages transport strategy. API-key
requests continue to use the unmodified Anthropic wire format. Tool execution,
permissions, schemas, arguments, and stored tool names remain evorch's contracts.

The reference is OMP commit
[`470b7ef`](https://github.com/can1357/oh-my-pi/tree/470b7efb67673960d37e3e64584669e6e9caad1c)
and locally inspected Claude Code **2.1.295**, built on 2026-10-08. The current
profile uses its SDK **0.128.0** and runtime **v26.3.0**. OMP at the reference
commit pins CLI 2.1.280 and SDK 0.112.1; these older constants were not copied.

## Compatibility boundary

| Surface | Subscription behavior |
| --- | --- |
| Authentication | Bearer token, `/messages?beta=true`, redirects disabled |
| Identity | CLI User-Agent and system identity; Stainless headers; persistent installation seed and account-scoped device ID; matching metadata/session header |
| Tools | OMP `_` prefix; evorch-specific tools have an inner MCP-style namespace |
| Arguments and schemas | Original evorch definitions; no `path`/`file_path` translation |
| System | Billing control block, CLI identity, original instructions |
| Body | CLI field order; checksum patches the final serialized bytes |
| Caching | One-hour system/tool/message markers; original history retained |
| Thinking | Explicit model-validated adaptive/budget controls and keep-all context management |
| Compression | gzip, deflate, Brotli, and Zstandard accepted and decoded |
| Transport | OAuth-only BoringSSL client, captured TLS ClientHello, HTTP/1 header casing/order, no hidden retries |
| Correlation | Attempt UUIDs, request class, truthful prompt provenance, previous successful response IDs |

The beta profile contains the common inference defaults and extended-cache TTL.
Effort and thinking-display-updates are advertised only when their fields are
sent; cache-diagnosis accompanies the diagnostics envelope. Optional OMP features such as
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
profile. The reference Linux ClientHello is 1,504 bytes. Local comparison
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
digits. `Reply LOCAL_CAPTURE_OK.` with version 2.1.295 produces `bef`.

The native 2.1.295 checksum differs from OMP's older whole-body algorithm.
It uses XXHash64 seed `0x4d659218e32a3268` and the low twenty bits, excluding
literal model values and routing fields (`max_tokens`, `fallbacks`, and
`fallback_credit_token`) **from hashing only**. The transmitted fields are kept.
Only the five digits in the adapter's first billing block are replaced; sentinel
strings in user text, tool schemas, arguments, and historical results stay intact.

A local HTTPS capture of the installed native CLI produced a 63,019-byte
request with checksum `bf7d5`. Restoring its billing placeholder and applying
the native exclusions reproduced `bf7d5` with both Bun and Rust; OMP's older
whole-body calculation produced `728b0`. Fixed independent synthetic vectors
are checked into the fingerprint tests. The captured CLI prompt and bundled
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

Anthropic's [current SDK and third-party app guidance](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)
states that those clients can draw from subscription limits. Its
[usage-credit guidance](https://support.claude.com/en/articles/12429409-manage-usage-credits-for-paid-claude-plans)
describes paid overflow after included limits are reached and usage credits are
enabled. This compatibility adapter does not guarantee billing eligibility or
change the account's usage-credit settings.

## Updating the profile

1. Pin the new CLI version and OMP commit. Inspect an isolated installation;
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
