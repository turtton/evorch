# T0 shared API handoff

All paths below are relative to the repository. Shared signatures/types/module
exports/Cargo are owned by **E** after T0. Request changes from E rather than
editing them concurrently. The exact Rust declarations are the API authority.

## Configuration (A)

config::mcp exports load_project(&Path) -> Result<ProjectMcpConfig, McpConfigError>
and save_override(&Path, SourceRevision, McpOverrideMutation) with the same result.
Implementation modules: load, expand, validate, save. Shared types are in
crates/config/src/mcp/types.rs. Both entry points currently fail Unavailable with
zero I/O; replace these stubs, not the contracts. Config remains a leaf crate.

- ProjectMcpConfig holds canonical_root, revision, ordered servers, a fatal
  preparation_blocked bit and safe diagnostics. Each McpServerEntry has origin,
  imported_exists, optional normalized definition, tombstone and diagnostics.
- McpOverrideMutation::{Upsert,Disable,RemoveOverride} means complete TOML
  replacement / tombstone / reveal lower definition. NEVER write .mcp.json.
- Template(Vec<TemplatePart>) retains Literal and Env{name,default}; ValueSource
  is Template or Keychain{name}. This AST's serde form is **internal**, not the
  user TOML/JSON shape: A must parse/render actual source syntax. Debug is opaque.
- Roles are config_key strings, omission -> worker, empty -> unpublished.
  MCP_ROLE_KEYS mirrors agents::Role::config_key without a cyclic dependency;
  runtime's contract test guards the correspondence. Unknown roles fail.
- SourceRevision::from_sources hashes domain-separated presence + length + bytes
  of JSON and TOML, distinguishing absent from empty and field-boundary changes.
  Recheck both files before atomic save and refuse symlinks/conflicts.

Initial bounds (inclusive maxima; exceeding is a classified failure): config 1
MiB, 64 servers, 64 KiB string, timeout 1..=600000 ms; defaults startup 10000 ms,
request 60000 ms. These constants are in config::mcp.

## Secrets / approval (B)

Shared declarations: crates/tools/src/mcp/types.rs. Implement in approval.rs and
secrets.rs. McpCredentialBackend is Available(Arc<dyn sandbox::CredentialStore>)
or Unavailable(SecretUnavailableReason). Resolved values are sandbox::Secret;
there is no keyring type or keychain feature requirement in tools/runtime.
The binary composition edge calls sandbox::open_keychain_only; the T0 factory
returns KeychainUnavailable without probing or creating files. B supplies the
real implementation and never delegates to open_default/plaintext fallback.

McpSecretResolver::resolve receives scope, ValueSource and an explicit environment
lookup, returning redacted Secret. Derive keys from MCP namespace + canonical
project identity + server + logical name; never accept a provider key directly.

McpApprovalStore::{preview,approve,revoke,check} is the ledger boundary. It accepts
a fresh McpApprovalInspection (B projection + explicit C destination inspection),
not a resolver hidden behind synchronous ledger calls. preview/check perform no
spawn/connect/secret resolution. approve receives both the UI confirmation and
fresh current inspection; compare both to the pending preview. The runtime GUI
backend owns async destination inspection outside the render thread.
ApprovalScope = canonical project root + server ID. ConfirmMcpApproval carries
preview ID AND expected SourceRevision AND expected ApprovalFingerprint. Re-read
current sources/projection at confirmation and before execution. Summary DTOs
contain only safe paths/destination scheme+host+port/reference names and warning
codes, never arguments/query/header/credential values. previous_fingerprint
supports showing a changed launch/destination. A preview cannot mint permission.
ApprovalPermit is non-serde, opaque outside mcp, and revocation-aware. Only B's
approval implementation should mint it. No config flag/project trust/model
answer is approval. Missing/corrupt ledger fails closed; use user_config_dir.

ApprovalFingerprint::from_projection hashes B's canonical projection with a
separate SHA-256 domain. It is NOT the config hash: preserve literals and secret
reference names/defaults; exclude resolved secrets, inherited environment values,
roles and transient state. PATH resolution changing executable changes projection.
DefinitionFingerprint::for_definition is a third nominal type/domain and hashes
remote name/description/input schema (including input annotations).

## Transport / protocol (C/D)

crates/tools/src/mcp/transport/api.rs exports McpTransport (exchange, notify,
respond, cancel, shutdown), McpTransportFactory::connect, RpcId, RpcRequest,
RpcNotification, RpcFrame, McpResponseEnvelope, ProtocolEra/Version, CancelContext,
CancelRequest, ShutdownReason and McpProcessLimiter/Permit.

- Factory requires ResolvedMcpServer + ApprovalPermit + shared limiter + cancel.
  Check scope/fingerprint/revocation **before** spawn, DNS, secret transmission
  or connection. ResolvedMcpTransport is non-serde with redacted Debug. HTTP also
  requires non-serializable ApprovedMcpDestination (no public mint in T0).
- exchange/notify/respond/cancel/shutdown take &self: do not hold the call mutex
  to send a cancellation. respond is for legacy unsupported server-request errors
  only; modern MUST NOT send responses. Late probe IDs are quarantined.
- RpcFrame is a structured envelope, NOT a strict network parser. C must validate
  jsonrpc version, mutually exclusive fields, IDs, methods and result types in
  codec.rs before admitting it. No raw frame/cause flows into safe McpError.
- Envelope retains HTTP status + needed headers + frames, including 400 modern
  errors. Limits are checked by its constructor; streaming/parser layers enforce
  limits before accumulation. Return from SSE at the matching final response.
- Shared cancellation_action is fixed by cancellation.json. Sticky CancelContext
  uses watch, no missed wakeup. Flush/cancel before stdio shutdown; closing modern
  HTTP is cancellation. No replay of OutcomeUnknown calls.
- C edits network_guard/{mcp,stream}.rs, which can extend NetworkGuard via new
  impl blocks: no mod.rs ownership is needed. Existing post_json/get are unchanged.
- D reuses sandbox process launch policy and holds the project admission permit
  until process/descendants are reaped; constructor/config loading is inert.

Bounds in mcp::types: request/frame 5 MiB; list <=100 pages, <=1000 tools, <=8 MiB;
4 live stdio per project; 4 HTTP preparations; 30000 ms total preparation deadline;
1 restart per run/server; public name <=64 bytes. Oversize never means truncation
and success. Header/frame-count envelope bounds (16 KiB / 1024 frames per exchange) are
also named constants there.

## Runtime / GUI / integration (E/F1/F2/G)

runtime::mcp (api.rs) exports McpRunService::{prepare_new_run,restore_snapshot},
PrepareMcpRun, RestoreMcpRun, RunMcpLease and McpGuiService. gui::mcp reexports the
GUI boundary only. T0 UnavailableMcpService is **not wired into composition** and
performs no I/O. E supplies the actual backend after A/B/C/D and wires the binary
keychain-only store. benchmark must branch before invoking any MCP service.

- RunMcpLease owns sessions and immutable FrozenMcpSnapshot. New-run preparation
  filters role/enabled/approval before resolving secrets/admission/discover. One
  failed server is excluded, not a failed agent run. Admit server IDs in order.
- RestoreMcpRun includes role/run/root/cancel plus saved snapshot; synchronous
  restore does not connect. First call revalidates current role/enabled/consent,
  destination and matching definition. E must add the snapshot to the strict
  RunRestoreDescriptor, executor recreation and continue path without prefix churn.
- ProjectMcpLimiter is a non-waiting, project-keyed shared semaphore primitive
  implementing McpProcessLimiter. Share one instance across runs/GUI/lazy restore.
  Caller supplies canonical root; the limiter itself does not access filesystem.
- FrozenMcpTool stores ordered public name -> RemoteToolKey binding, original
  definition, approval fingerprint and definition fingerprint. No reverse parse.
  No credential, session ID, permit or process is serializable in the snapshot.
- McpGuiService has settings/save_override/subscribe_status/preview_approval/
  confirm_approval/revoke_approval/test_connection/save_secret. Background execution
  only; status subscription is project-scoped watch state and uses safe DTOs.
  Opening/saving settings never launches/connects. Connection testing requires the
  same approval and limiter as runs. Confirmation needs the full preview identity.
- SaveMcpSecret contains redacted Secret and expected revision + TOML mutation.
  Backend writes keychain first, saves only logical reference to TOML, and cleans
  only a key newly created by that failed operation. Do not log/store GUI commands.
- Status enum: Disabled / Invalid / NeedsApproval / SecretUnavailable / Connecting /
  Ready / Unavailable / ProcessLimitReached. McpStatusEvent includes ApprovalScope
  and optional classified error only. No transient status in tool descriptions.

The existing McpClient/McpToolRegistry API is deliberately kept during T0 so
existing tests compile. C may rework client/protocol under the shared contracts;
E owns registry/tool migration and the four existing registry integration tests.
wire.rs's four helpers, search/mcp.rs and search/envelope.rs are frozen. The safe
McpError::detail JSON shape remains server/method/request_id/class/status, with
classified new variants only. E owns error.rs changes.

T0 tests check the contract primitives, fixture driver, inert service and search
helper behavior. They do NOT assert that unimplemented production transports,
approval persistence or runtime integration already work. C/D/B/E/F/G add the
behavioral tests in TEST-MAP.md; do not replace them with copied instructions.

## E pure registration layer

`mcp::names::McpNameBindings::new` consumes the complete accepted remote-key set.
It exposes `public_name`, `remote_key` and public-name-sorted `iter`; it never
sanitizes or reverse-parses. Normal-name collisions hash **both** tuples. The
hash input is `evorch.mcp.external-name.v1\0`, then each UTF-8 tuple component
prefixed by its unsigned 64-bit big-endian byte length. SHA-256 is base64url,
without padding. Final collisions/duplicate remote keys are errors.

`FrozenMcpSnapshot::from_servers(McpServerTools)` assigns names after client-side
schema exclusion and sorts once. `McpToolRegistry::from_snapshot(snapshot,
connector)` checks saved bindings/fingerprints without I/O and preserves their
saved order, including a historical hash whose collision partner is absent.
`tools()` registers those definitions; `snapshot()` returns them unchanged.

The new exported `McpRegistryConnector`/`McpRegistrySession` interfaces separate
pure registration from live runtime authority:

- `tool_transport` is pure metadata. Stdio declares filesystem/process/network
  capabilities; HTTP is network-only. Hints never lower these permissions.
- `check_execution` receives the saved `FrozenMcpTool` and must check current
  enabled/role/consent/destination. The registry calls it before lazy connection
  and immediately before every remote call. No config flag mints consent.
- `connect` must use the approved factory/shared limiter/run cancellation and
  enforce restart limits. It may return a session already prepared for this run,
  but must not share discovery between runs. The connector/run lease owns
  transport cleanup, including lazy sessions created after restore.
- `definitions()` returns already-discovered metadata without I/O. A call checks
  the used definition's full fingerprint (name, description, input schema).
  Changed/missing definitions reject the call; no schema/history is rewritten.
- `call_tool` receives the original bound remote name, never the public name.
  It returns `(McpToolResult, safe diagnostic detail)`. Calls are never retried.
  `isError` remains tool failure with `tool_rejected`, not transport failure.

E2 installs `runtime::mcp::ProjectMcpService`. Its constructor is inert and takes
an injected `McpCredentialBackend`, `McpApprovalStore` and `NetworkGuard`. The same
service owns `ProjectMcpLimiter` (4 stdio processes); `limiter()` is shared with
GUI testing/lazy restore. `with_transport_factories` is a trusted host/test seam,
not a model API. Preparation loads config, filters enabled/role before authority
or secret access, checks approval, resolves secrets, negotiates/lists with configured
timeouts, then freezes names/schemas. Admission is server-ID ordered and sequential
(HTTP concurrency 1), with a 30-second total preparation budget. Only failed servers
are excluded. Broken project TOML is classified even with no lower-priority servers.

`AgentRuntime::with_mcp_service` injects the backend without doing I/O. New-run
preparation occurs after executor rebuilding and before provider tool specs; only
that run's concrete MCP names are granted. `RunMcpLease::register_executor` forks
an executor without adding run tools to host-tool inheritance. Benchmark paths do
not call the service at all. Status watch state and diagnostic events carry only
safe DTOs; no transient status changes schemas/descriptions. Every call rechecks
current enabled/role/approval/destination. The native stdio session retains its
concrete supervisor: after an uncertain failed call is returned, a later explicit
call may restart once, renegotiate and compare fixed definitions. The failed call
is never replayed. Lease shutdown awaits transport cleanup before the run ends;
Drop/run cancellation also owns cleanup.

The old registry constructor/discover adapter is removed. Runtime scope tests use
frozen bindings, real approved HTTP transport and modern discovery, preserving
scope/per-tool denial before I/O, one CLI approval, lifecycle events and safe
five-key diagnostic detail. No wildcard/test alias bypasses the role boundary.
Runtime restore/continue wiring (E3) and GUI backend/binary injection (E4) remain
separate work; this backend's restore method currently fails closed.

Secret failures remain distinct: SecretUnavailable / SecretLocked / SecretDenied /
SecretMissing. MissingEnvironment(McpValueField) carries only a fixed field enum
(command/args/cwd/env/url/headers/token), never variable names or values. The
McpError::detail five-key shape is unchanged; GUI diagnostic wording covers these
classifications.

### Existing test migration checklist (live integration stage)

| Existing test | Replacement and invariant that must remain |
| --- | --- |
| `tools/tests/executor.rs` optional-null MCP validation | Frozen fake registry, concrete public name from binding. Both validation outcomes leave connector/call counts zero. Keep `reads_null_as_omitted = false`. |
| `tools/tests/apply_patch_reserved_name.rs` | Keep direct `ToolExecutor` reserved-name denial for a scope-gated non-builtin impostor, add forged frozen-binding rejection, and verify remote `apply_patch` is namespaced without replacing the builtin. Keep host-inheritance coverage. Do not simply delete the raw-name denial. |
| `runtime/tests/mcp_scope_e2e.rs` | Move registration to frozen bindings/concrete role allowlist. Preserve scope/policy denial with zero I/O, approval provenance/order, and the real guarded transport diagnostic scenarios; update fixture IDs only when moving to modern-first transport. |
| `runtime/tests/mcp_scope_diagnostics/mod.rs` | Reuse migrated scope harness; preserve safe classified status/server/request correlation and absence of URL/header/body/credential material. |

Shared follow-up additions: `McpErrorKind::ConfigConflict` is distinct from
`StalePreview`; runtime `mcp_config_error_kind` maps configuration save conflicts
without retaining raw causes. GUI reexports this classification through runtime,
not a regular tools dependency. `McpToolContent::ResourceLink` has the exact MCP
wire tag `resource_link`.

Tool results preserve structuredContent as JSON text. Return-time output bounding
uses the existing tools output artifact contract (the executor does not re-cut an
already-bounded result). Audio and embedded resources retain only already-returned
payload bytes in local artifacts, with bounded MIME/URI/size explanations. Resource
links are references only; no URI is fetched. Metadata is JSON-quoted and capped
at 512 UTF-8 bytes per field; long links retain their full reference in an artifact.
At most 8 media/reference artifacts and the existing 4 images are retained per
result, and an exceeded ceiling is explicitly reported. Artifact failure is safe
and visible; raw filesystem errors/base64 payloads are not copied into the model.
