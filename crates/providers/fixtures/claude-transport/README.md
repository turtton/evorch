# Claude transport reference

Captured from official Claude Code native binaries on Linux x86_64, using a
local HTTPS server and CONNECT proxy with dummy OAuth credentials and an isolated
network namespace. No Anthropic model endpoint was contacted. These fixtures were
extracted from the native client captures, independently of the Rust transport
implementation.

`native-2.1.322-client-hello.bin` comes from `@anthropic-ai/claude-code-linux-x64`
2.1.322 (native binary SHA-256
`bfb3703ad5380242328459bc62ada4bde8ae7ba57c6e1eeadb9c00e08874c478`). It contains the
complete 1508-byte TLS record. All six captured connections (interactive, print, tool
continuation, search) normalized to the same bytes. Only the ClientHello random,
session identifier, and key-share public key bytes are zeroed, by a standalone
script on the captured bytes. Their lengths, the SNI (`api.anthropic.com`), TLS record and handshake
headers, cipher suites, extensions, extension order, and all other bytes remain
unchanged. Compared with the 2.1.295 capture (1504 bytes) the only difference is two
additional `signature_algorithms` entries (`0x0807` Ed25519 and `0x0603` ECDSA
P-521/SHA-512) before RSA PKCS#1 SHA-1. The test normalizes an actual Rust TLS
connection using a separate parser before comparing the entire record.

`native-2.1.295-shared-headers.json` was captured from 2.1.295; the 2.1.322 header casing and
order are the same except for `x-claude-code-sandy-plover`, which evorch intentionally does
not send (see docs/claude-compatibility.md). It contains the observed HTTP/1 header names in
wire order, preserving spelling and case. Per-attempt prompt/request identifiers
are excluded because this fixture covers the shared transport headers. Values,
request bodies, credentials, and account identifiers are not included.

`native-2.1.295-child-headers.json` comes from a separate interactive capture
that invoked an Agent and then an Agent inside that child. A first-level child
sends its agent ID; the nested child also sends its parent's agent ID. Root
requests send neither. The fixture includes per-attempt header names, but omits
`x-claude-code-agent-type`: that value names a native agent catalog entry, which
evorch does not claim. It contains no header values or request bodies.
