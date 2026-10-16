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
| Identity | CLI User-Agent and system identity; Stainless headers; matching metadata/session header |
| Tools | OMP `_` prefix; evorch-specific tools have an inner MCP-style namespace |
| Arguments and schemas | Original evorch definitions; no `path`/`file_path` translation |
| System | Billing control block, CLI identity, original instructions |
| Body | CLI field order; checksum patches the final serialized bytes |
| Caching | One-hour system/tool/message markers; original history retained |
| Thinking | Explicit model-validated adaptive/budget controls and keep-all context management |
| Compression | gzip, deflate, Brotli, and Zstandard accepted and decoded |

The beta profile contains the shared OMP agent defaults and extended-cache TTL;
effort is advertised when thinking is enabled. Optional OMP features such as
fallback credits, fast mode, deferred tools, server compaction, and tool-search
controls are not enabled by this adapter. No one-million-context beta is added.
evorch keeps its own account/profile identity derivation and original tool
definitions. The Rust HTTP/TLS client is retained; this is not a reproduction of
the native CLI's TLS ClientHello.

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

HTTP-boundary tests independently validate the billing suffix and final-byte
checksum before separating the first control block from the model input. The
existing strict prefix assertions then compare instructions, settings, tools,
schemas, and every prior message. They also reject deliberate mutations with
valid recomputed checksums. The general Anthropic cache contract is unchanged.

These tests protect input preservation. Synthetic cached tokens do not establish
the service's cache hit rate, subscription eligibility, or usage-credit routing.
Those remain service-side decisions and require live account validation.

## Updating the profile

1. Pin the new CLI version and OMP commit. Inspect an isolated installation;
   never use a real credential or the user's normal Claude configuration for
   captures. Direct a dummy token to a loopback mock. For the official-host
   branch, use a locally trusted TLS mock and a CONNECT proxy that rejects every
   destination except `api.anthropic.com:443` and forwards it only to loopback.
2. Compare CLI/SDK/runtime versions, headers, beta selection, body ordering,
   cache markers, identity, model controls, and raw checksums. Print mode uses
   SDK identity/entrypoint; distinguish those from the CLI profile.
3. Update `profile.rs` and the independently derived checksum vectors together.
   Compare send, fragmented SSE, interleaved tools, signed thinking, replay,
   token rotation, and client recreation; verify API-key requests remain intact.
4. Run provider tests, runtime subscription cache tests, and
   `scripts/check-cache-contracts.sh`. Preserve the general prefix checker and
   negative mutation cases. Complete Clippy and formatting checks before release.

The profile is immutable during a run. A server minimum-version rejection
reports that an update is needed, with a sanitized required version. evorch does
not adopt a new User-Agent/billing version midway through a warm conversation.
