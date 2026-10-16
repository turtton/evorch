# Claude transport reference

Captured from the official Claude Code native 2.1.295 binary on Linux, using a
local HTTPS server and CONNECT proxy with dummy OAuth credentials. No Anthropic
model endpoint was contacted. These fixtures were extracted from the native
client capture, independently of the Rust transport implementation.

`native-2.1.295-client-hello.bin` contains the complete 1504-byte TLS record.
Only the ClientHello random, session identifier, and key-share public key bytes
are zeroed. Their lengths, the SNI (`api.anthropic.com`), TLS record and handshake
headers, cipher suites, extensions, extension order, and all other bytes remain
unchanged. The test normalizes an actual Rust TLS connection using a separate
parser before comparing the entire record.

`native-2.1.295-shared-headers.json` contains the observed HTTP/1 header names in
wire order, preserving spelling and case. Per-attempt prompt/request identifiers
are excluded because this fixture covers the shared transport headers. Values,
request bodies, credentials, and account identifiers are not included.
