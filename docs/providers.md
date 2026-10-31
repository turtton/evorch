# Claude and Cursor providers

For Codex (ChatGPT login) and OpenAI API image generation and editing, see
[Image generation](image-generation.md). Only `openai-codex` and `openai` expose
this optional capability; it uses a separate image-model chain, not chat tools.

Open **Provider settings** to add a Claude API, Claude subscription, or Cursor
profile. Claude API keys can be read from an environment variable or saved in
the credential store. Subscription profiles support browser sign-in; Cursor also
supports a user API key. Use **Refresh
models** after signing in, select the models to enable, choose a default, and
save the profile. Routing settings can then select the profile.

Claude subscription sign-in uses a local callback. If the browser cannot reach
this machine, paste the final redirect URL or authorization code into the login
dialog. Cursor browser sign-in waits for the browser authorization to finish.

For Cursor token authentication, choose **API key**, create a **user API key** at
[Cursor Dashboard → API Keys](https://cursor.com/dashboard/api), paste it into
**Cursor API key**, and click **Authenticate with API key**. This uses your Cursor
plan, not an OpenAI or Anthropic API account. evorch exchanges the key for a native
agent access token and re-exchanges it automatically when that token expires.
The key and access token are kept together in the credential store; the masked
input is cleared when login starts, is cancelled, or the editor closes. Existing
browser credentials continue using OAuth refresh. Model discovery, chat, and
quota requests use the exchanged access token, never the API key directly.
See Cursor's [CLI authentication](https://cursor.com/docs/cli/reference/authentication)
and [documented token exchange](https://cursor.com/docs/rollouts#send-deployment-events).

The footer displays each subscription's remaining quota, with reset information
on hover. Claude shows the five-hour and weekly windows advertised by the
account, plus extra usage charges when enabled. Cursor displays its separate
Cursor Models and Other Models pools, on-demand usage when available, and legacy
request counters when the newer summary is unavailable. Missing limits remain
unknown. A failed refresh keeps
the previous snapshot marked stale and does not interrupt chat.

Claude API keys do not provide the Claude Pro/Max subscription quota endpoint.
The normal request usage and cost views remain available for API-key profiles.

Claude subscription requests use OMP's custom-tool prefix: `read`, `write`,
`edit`, `grep`, and `shell` appear as `_read`, `_write`, `_edit`, `_grep`, and
`_shell`. Other evorch tools use `_mcp__evorch__<name>` with their original
schemas. Argument names and descriptions retain the actual host behavior.
These names are a
provider presentation convention; execution stays in evorch. Names that cannot
fit Anthropic's tool-name constraints use a deterministic hashed MCP name.
Responses and saved tool-call history are translated at the provider boundary,
so internal names, execution, and approval checks use the original tools.
Claude API-key profiles retain their original tool names and argument schemas.
The subscription transport uses a pinned Claude Code compatibility profile for
headers, identity, billing checksums, and one-hour cache markers. See
[Claude compatibility maintenance](claude-compatibility.md) for the reference
versions, validation, and differences from OMP.

## Transient overload recovery

Streaming chat requests retry `server_is_overloaded` failures even when Codex
reports them as HTTP 400, including `response.failed` SSE events. The default
policy makes at most three attempts in total: the initial request, a retry after
250 ms, and a final retry after another 500 ms. If overload persists, the failure
includes the attempt count and the last provider error. Cancelling the run also
cancels pending retries.

Other HTTP 400 errors and authentication failures still fail immediately. Rate
limits retain their existing quota handling. Low-level `send` and `stream` calls
do not gain retries; recovery remains in the shared `send_streaming` path.

Responses API streams (Codex and `openai`) begin with HTTP 200, so failures that
arrive as `response.failed` or top-level `error` SSE events are classified by
their code. Both dialects accept the same event shapes: an `error` event may carry
`code`/`message` at the top level or nested under `error` (nested fields win), and
`response.failed` carries them under `response.error`. A `retry-after` entry in
`error.headers` (integer seconds or an HTTP-date, name matched case-insensitively)
is read the same way as the `Retry-After` header of an HTTP 429 response, which
now also accepts an HTTP-date. The body of the resulting error always starts with
`code: message`, even when the status is synthesized.

- `openai`: `server_error` is handled as an HTTP 500 server failure and retried,
  and `rate_limit_exceeded` is handled as a rate limit without a delay. Other
  codes remain HTTP 400 errors.
- Codex follows the classes of Codex CLI 0.166.0:

  | Code | Result |
  |---|---|
  | `rate_limit_exceeded`, `slow_down` | rate limit; the delay comes from `error.headers`, else from a "try again in N s/ms/m" hint in the message |
  | `server_error` | HTTP 500 (retried) |
  | `policy_capacity_unavailable` | HTTP 503 (retried) |
  | `server_is_overloaded` | HTTP 400, retried as described above |
  | `insufficient_quota`, `credit_balance_exhausted`, `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`, `usage_not_included`, `usage_limit_reached` | HTTP 402, which routing treats as a quota failure |
  | `model_not_found` | HTTP 404 |
  | `context_length_exceeded` | HTTP 400; `ProviderError::is_context_window_exceeded()` identifies it (no runtime recovery yet) |
  | `flex_unavailable`, `invalid_prompt`, `cyber_policy`, `bio_policy`, `misalignment_policy_violation`, any unknown code | HTTP 400, not retried |

Semantic difference from Codex CLI: evorch ends the turn at the first failure
event. Codex CLI treats a top-level `error` event as terminal only for
`flex_unavailable` and `model_not_found` and otherwise keeps reading the stream;
evorch treats a top-level `error` with any code as terminal.

A `response.incomplete` with an unknown or missing reason finishes as
`Other`: executable tool calls are removed from the final response and logged
for diagnosis, so the runtime ends in Error without executing them or requesting
a continuation. Partial text, reasoning, and usage are retained. Known reasons
`max_output_tokens` (Length) and `content_filter` (ContentFilter) retain their
existing behavior, including tool execution when tool calls are present.

## Configuration

Only credential references are written to TOML; OAuth token families and Cursor
API key credentials are stored in the credential store and renewed automatically.
Cursor uses the same configuration below for either authentication method.

```toml
version = 3

[providers.claude-api]
type = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-sonnet-4-6"]
default_model = "claude-sonnet-4-6"

[providers.claude]
type = "anthropic-subscription"
base_url = "https://api.anthropic.com/v1"
credential = { type = "keyring", service = "evorch", account = "claude" }
models = ["claude-sonnet-4-6"]
default_model = "claude-sonnet-4-6"

[providers.cursor]
type = "cursor"
api_protocol = "cursor-agent"
base_url = "https://api2.cursor.sh"
credential = { type = "keyring", service = "evorch", account = "cursor" }
models = ["default"]
default_model = "default"
```

Use the authenticated model list to find the IDs available to your account.
Cursor's Auto wire ID is `default`. If the account's model list does not
advertise it, select an available model and set it as the profile default.
Cursor uses its native Connect agent transport; tool requests are handed to
evorch's runtime for execution.
Choose Cursor's thinking, effort, and fast variants from the refreshed model
list; the selected variant determines these settings. The chat model picker
therefore offers no separate effort step for Cursor.

Cursor usage is priced as an API equivalent (Cursor is a subscription).
Suffixes such as `-high`, `-fast` and `-thinking` are stripped from the model
ID, and the model family (`claude-*` → anthropic, `gpt-*`/`o*` → openai,
`gemini-*` → google, `grok-*` → xai) selects the models.dev entry whose prices
and limits are used. Cursor's own models (Composer, Grok 4.5/4.6/4.7) use the
rates published on [Models & Pricing](https://cursor.com/docs/models-and-pricing),
including the `-fast` lane; Cursor's documented context windows apply too.
Prices set on a model entry in the TOML take precedence, and the provider
settings show where each limit and price came from.

Claude supports text and tool calls, and preserves signed thinking returned by
the provider across tool continuations and saved history. Subscription profiles
support model-validated reasoning effort, including adaptive thinking on
Sonnet 4.6 and Opus 4.6/4.7; unsupported model/effort combinations fail before
inference. Agent Roles and the chat model picker offer the effort levels
supported by the selected subscription model. Models that require thinking
do not offer `none`, and unrecognized models have no effort selector.

API-key profiles (`type = "anthropic"`) send the same `thinking` and
`output_config.effort` mapping when a route or the model picker selects a
reasoning effort and the model supports it (the capability table is shared with
subscription profiles; Claude Code's `context_management` beta is not sent, so
no `anthropic-beta` header is needed). Nothing is sent when no effort is
selected, so default behavior is unchanged, and an effort the model does not
support (or an unrecognized model ID, such as a gateway alias) is omitted with
a warning instead of failing the request. Enabled thinking requires the default
temperature, so a custom `temperature` with a thinking effort is rejected
before inference.

Prompt cache: within a run, keep the model and effort fixed. Anthropic renders
the resolved effort and thinking configuration into the cached prefix, so
changing the effort between requests is an explicit cache boundary: the cache
restarts on the first request after the change, and continuations that keep the
new effort reuse it again. `cache_prefix_contract` and `claude_cache_e2e` pin
this contract. Switching efforts mid-run is not a history replacement.
See the [Anthropic stream contract](anthropic-stream-events.md)
for supported events and signature replay boundaries. Cursor does not expose
temperature, output-token limits, or service-tier controls.

The protocol implementation was informed by
[oh-my-pi](https://github.com/can1357/oh-my-pi/tree/602b6c812fa9ef774f359f1e399a09d30ee2eaca),
including its OAuth, native Cursor transport, and quota parsers. Offline mock
tests verify authentication, token rotation, tool continuations, and prompt
prefix preservation. Unknown top-level protobuf server fields are skipped for
forward compatibility, matching the reference client; a valid `turnEnded` is
still required, and unknown fields never authorize local tool execution.
Live account acceptance requires an actual account.

### OpenAI API

An `openai` profile calls the OpenAI Responses API (`POST /v1/responses`) with
the API key as a Bearer token. The shortest form names only the key's
environment variable:

```toml
version = 3

[providers.openai]
type = "openai"
api_key_env = "OPENAI_API_KEY"
```

Omitted fields default to `api_protocol = "openai-responses"`,
`base_url = "https://api.openai.com/v1"`, the `OPENAI_API_KEY` environment
variable, and evorch's built-in OpenAI model list and default model; any field
you set takes precedence. The key can instead be saved in the credential store
with `credential = { type = "keyring", service = "evorch", account = "..." }`.
A `base_url` without `/v1` has it appended, so `https://host` and
`https://host/v1` both send to `https://host/v1/responses`. Other
`api_protocol` values are rejected; use `openai-compatible` for Chat
Completions endpoints.

### Responses encrypted reasoning replay

Normal `openai` and `openai-codex` requests explicitly include
`reasoning.encrypted_content` and use `store: false`. Confirmed opaque reasoning
items are distinct from displayable reasoning summaries. Responses endpoints must
be HTTP(S) URLs without URL credentials, query parameters, or fragments; these
components cannot provide a non-secret replay identity. Items may only be replayed
to the same profile, endpoint, route model (including the `+fast` suffix), wire
model, speed and effective service tier. Returning to an earlier model or forking
a run must not reactivate older items.

Codex identifies the account using the non-secret ChatGPT account identity from
the same authentication lease used for the HTTP request. Refreshing tokens for
the same account does not create a replay boundary. API-key providers cannot
automatically detect an account replacement within the same profile: use a
**different profile for each account**, rather than replacing its external key
in place. API keys and access/refresh/ID tokens are never replay identities.

These items are not sent to Chat Completions or Anthropic wire formats. Official
Codex compaction uses the same replay eligibility rules; text summarization must
exclude them. Live API acceptance has not been tested for this feature.

GPT-5 and later models of `openai` and `openai-codex` profiles edit files with
the Codex-compatible `apply_patch` tool instead of `edit` and `write`. The
`edit_tool` key of a profile or model entry changes this for any provider; see
[apply_patch](apply-patch.md).

## Failures and cooldowns

Each failed request records its failure kind and a short error detail in the
`RequestFailed` event. The detail is redacted (the profile's credential and
any secret-shaped text become `***`) and limited to 1 KiB, so a quota message
remains visible in the transcript and diagnostics after the run ends.

Failures are classified for routing as follows:

| Failure | Kind | Cooldown scope | Duration |
| --- | --- | --- | --- |
| HTTP 402 | quota | whole profile | `Retry-After`, else `quota_secs` |
| Kimi subscription HTTP 403 whose body reports a usage limit | quota | whole profile | `Retry-After`, else `quota_secs` |
| Other HTTP 401 / 403 | auth | whole profile | until settings are reloaded |
| HTTP 429 / rate-limit response | rate limited | that profile and model | `Retry-After`, else `rate_limit_secs` |
| HTTP 5xx, timeout, other | — | none | — |

While a profile or model is cooling down, routing skips it for every run, not
only the run that saw the failure. A `CooldownStarted` event records the
target, cause, and duration; there is no matching end event. A cooldown is
never shortened by a later, shorter failure. Reloading settings recomposes the
routed model and clears every cooldown.

The default periods can be changed under `[routing]`; a role profile's
`routing` table accepts the same keys:

```toml
[routing.cooldown]
quota_secs = 1800      # default: 30 minutes
rate_limit_secs = 30   # default: 30 seconds
```

## Model chains and fallback

A route under `[routing.routes]` is a pool of paths to the same model: each
candidate names a profile and, optionally, the model ID that profile uses for
it. A role lists routes in priority order under `models`, and each entry sets
its own `reasoning_effort`:

```toml
version = 3

[[routing.routes."moonshotai/kimi-k3"]]
profile = "kimi"          # subscription
model = "k3"
[[routing.routes."moonshotai/kimi-k3"]]
profile = "neuralwatt"    # pay-as-you-go path to the same model
model = "kimi-k3"

[[routing.routes.claude]]
profile = "claude"

[agents.roles.planner]
models = [
  { route = "moonshotai/kimi-k3", reasoning_effort = "high" },
  { route = "claude", reasoning_effort = "medium" },
]
```

A role without `models` uses the route named after the role. A worker or
reviewer category without `models` inherits the role's chain. Strict loading
rejects a chain entry that names a missing route; a normal load warns and the
role fails when it is used.

When a request fails at one member of the chain, routing tries every other
member once: first the rest of the same pool, then the later pools, then the
earlier ones. Members that are cooling down are skipped, and a pool whose
members have all failed or are cooling down is left for the next one. The
member that failed first is retried last, and only when its failure was
transient (rate limit, timeout, transport, HTTP 408/429/5xx). When the chain is
exhausted the request fails with every attempt listed.

If a pool runs out on a transient failure whose `Retry-After` is short, the
request waits once and retries that pool before moving on. The limit is set
under `[routing]`:

```toml
[routing.fallback]
max_retry_after_wait_secs = 30   # default
```

The member that answers becomes the run's pin, and later requests in the run
start there instead of the chain head; the pin never moves back on its own.
Pins are saved with the run's snapshot, survive a restart or a settings reload,
and restoring a run whose pinned member is unavailable falls through to the
next member as above. A new run starts from the head of the chain.

### Compaction across a switch

Moving a run to another member discards the provider's prompt cache, so the
switch is also where the history is prepared for the new member. Before the
first request is sent there:

- A checkpoint made by official compaction (an opaque Codex blob) is
  re-summarized from raw history when the new member cannot replay it. The
  blob itself is never read or forwarded.
- A change of editing interface (`apply_patch` versus `edit` and `write`)
  always summarizes the old history as text, **even without a checkpoint**.
  This includes editing calls after an existing checkpoint, so the next
  agent request never replays the old interface's calls raw. It applies to
  fallback, explicit model changes, and settings reloads that change the
  override for the same profile/model. Each such fallback adds one summary
  invocation (and a model call when using the model summarizer). Same-interface
  fallback does not add this summary, and a first request without earlier
  history does not summarize. See
  [apply_patch](apply-patch.md#history-and-model-switches).
- The history is compacted when it is above the new member's context window
  × `[compaction].threshold`.

These use the existing `ModelSwitch` compaction boundary before the next agent
request. Later requests preserve and reuse the new prefix; a restored run
whose pinned member is unavailable goes through the same steps. Settings
reloads retain the prior editing interface. If that record is absent after a
restart/resume, an opaque checkpoint triggers a conservative re-summary; with
no checkpoint, only an earlier incompatible `apply_patch`/`edit`/`write`
`ToolUse` triggers one. Incompatible visible editing calls are checked even
when a prior-interface record exists, so a failed re-summary remains a required
boundary on resume. Model-switch checkpoints and compactions derived from them
persist an optional `protects_history` marker: later compaction operates on
the projected history and cannot re-expose the replaced raw calls/results.
Ordinary checkpoints omit this marker and retain their existing serialized
bytes and compaction input.

Summaries use official compaction when the run's member supports it, then the
chain, then the structural summary, which needs no model. Ordinary structural
summaries are lossy; when recovering a protected checkpoint, structural fallback
explicitly retains its readable body verbatim, independently of recent-message
selection. If that body exceeds the summary byte limit or the prepared context
cannot fit the target window, preparation stops without committing the candidate
checkpoint, replay scope/floor, or context-usage baseline. The previous durable
context remains available for retry; hidden raw tool batches are never restored
as a fallback.
A summary skips members whose window cannot hold its input and never moves
the run's pin. To summarize with a different chain, set
`agents.roles.compactor`; it is never started as an agent, and without
`models` the running role's chain is used:

```toml
[agents.roles.compactor]
models = [{ route = "claude" }, { route = "moonshotai/kimi-k3" }]
```

Version 2 files are migrated when loaded: `logical_model` becomes a
single-entry `models` list, and a route candidate's `reasoning_effort` moves to
the chain entries that use that route. The first saved change rewrites the file
as version 3.

### Editing chains in the GUI

Agent role settings edit the whole chain of a role or category. The first
picker sets the head route; each fallback entry below it picks one of the
defined routes and its own reasoning effort, and can be moved up, moved down
or removed. "Add … fallback" appends the first defined route the chain does
not use yet. Agent roles, categories and the compactor are offered only routes
of chat models, and the Image generator only routes of image generation models
(see [Model kinds and the image generator](#model-kinds-and-the-image-generator)).

### Output limits

For Anthropic Messages, OpenAI Chat Completions, and OpenAI Responses, each
request resolves its output limit from explicit role/category
`generation.max_tokens`, the selected
model's preset `max_output_tokens`, then models.dev. Positive metadata values
are used; unknown limits retain the provider's default. The selected profile
and model are resolved again on fallback. A `+fast` variant uses its exact
configured preset when present and its base model for catalog lookup.
Codex and Cursor do not automatically receive this limit because their wire
formats do not support that generation field.
`openai` profiles (OpenAI Responses) send this limit as `max_output_tokens`.
`openai-compatible` profiles (OpenAI Chat Completions) whose `base_url` is the
official OpenAI HTTPS endpoint (`https://api.openai.com`, port 443) send this
limit as `max_completion_tokens`, including reasoning tokens. Other compatible
endpoints and Kimi retain `max_tokens`.

### Automatic pools

Saving a provider in the GUI adds its models to the pools that serve the same
model. A model's identity is its models.dev `canonical_model_id` together with
its tier: `+fast`, or a `fast`, `highspeed`, `flex`, `eco` or `free` segment of
the provider's model ID, and its kind (`chat` or `image_generation`). Pools
never mix tiers or kinds. The catalog entry is found
through the model's `metadata_ref`, then the provider type's models.dev
providers, then the profile name, then a unique match across the catalog.
Models whose metadata is `manual` or `provider-default` are never grouped.

A pool's identity comes from its configured candidates, which must all resolve
to the same identity. Only pools that some role or category uses, including
the route named after a role without `models`, are considered, and a pool that
already has a candidate from the saved profile is left alone. When one enabled
model of the profile matches, it is appended at the lowest priority with
`auto = true`:

```toml
[[routing.routes."moonshotai/kimi-k3"]]
profile = "neuralwatt"
model = "kimi-k3"
auto = true
```

When several enabled models match, the GUI asks which one to add; a skipped
choice is not asked again in the session. Only a pool defined in the file
being saved is changed. When the pool comes from another layer (`config.d`,
`EVORCH_*`, the command line), the GUI reports that the model was not added
and leaves the file alone. Routing settings mark these candidates `auto`, and
they can be moved or removed like any other.

The addition is written once and is not recomputed on load. When models.dev
later lists an `auto` candidate under another identity than its pool, the GUI
warns once per session and leaves the configuration unchanged.

## Model kinds and the image generator

Each model of a profile has a kind: `chat` for conversations, or
`image_generation` for image generation ([ADR 0038](../intents/evorch/decisions/0038-image-generation.md)).
A model entry may set it explicitly; otherwise it is inferred from the model
ID. IDs starting with `gpt-image-`, `chatgpt-image-` or `dall-e-` (ignoring
case) are `image_generation`, and every other ID is `chat`. An explicit `kind`
always wins, so a model the inference gets wrong can be corrected:

```toml
[providers.openai]
type = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
models = [
  "gpt-5",                                                   # inferred: chat
  "gpt-image-1",                                             # inferred: image_generation
  { id = "house-painter", enabled = true, kind = "image_generation" },
  { id = "gpt-image-notes", enabled = true, kind = "chat" },
]
default_model = "gpt-5"
```

Existing files need no migration: entries without `kind`, including plain
strings, keep loading and are inferred. A route candidate's kind is that of the
model it sends to: an explicit `model` uses that model's entry on the profile
(or the inference when the profile does not list it), and a candidate without
`model` uses the profile's `default_model`. A profile's `default_model` must
therefore be a chat model.

The kinds are checked in both directions:

- Every candidate in the chains of agent roles, categories and
  `agents.roles.compactor` must be `chat`. This includes a role without
  `models` that uses the route named after it.
- Every candidate in the `agents.roles.image_generator` chain must be
  `image_generation` and belong to an `openai-codex` or `openai` profile.
- One route never mixes chat and image generation candidates.

Strict loading rejects a violation with the chain or route path, the route and
the model. A normal load only warns, and the affected chain then fails when it
is used: before a conversation or summary request is sent, the runtime refuses
a chain that contains a non-chat member with a typed error, so no prompt
reaches an image model. The chat model picker never lists image models.

`agents.roles.image_generator` is a chain like `agents.roles.compactor`: it is
never started as an agent and accepts no category. With no `models`, image
generation is disabled; there is no fallback to a route named after the role.
For example, generate with the Codex subscription and move to an OpenAI API key
profile once its usage limit is reached:

```toml
[providers.codex]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = { type = "keyring", service = "evorch", account = "codex" }
models = ["gpt-6-astra", { id = "gpt-image-2", enabled = true, kind = "image_generation" }]
default_model = "gpt-6-astra"

[providers.openai]
type = "openai"
api_protocol = "openai-responses"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
models = ["gpt-5", "gpt-image-2"]
default_model = "gpt-5"

[routing.routes]
codex-image = [{ profile = "codex", model = "gpt-image-2" }]
openai-image = [{ profile = "openai", model = "gpt-image-2" }]

[agents.roles.image_generator]
models = [{ route = "codex-image" }, { route = "openai-image" }]
```

The image generator only selects models in this release; the `generate_image`
tool and image provider capabilities that use the chain are described in ADR
0038 and will be implemented separately. Both `openai-codex` and `openai`
profiles can be composed by the runtime; composing an `openai` profile for chat
uses the Responses API, not the image generation endpoint.

In the GUI, models added to a profile, whether typed in or picked from the
fetched list, are saved with their inferred kind written out, and each model row
has a kind picker. Models already in the file keep the form they were written
in. Image models are never offered as the default model. The Codex catalog does
not list image models, so the Codex editor offers `gpt-image-2` as a separate
addition. Agent role settings have an Image generator section next to the
compactor, showing "Disabled (generate_image unavailable)" while its chain is
empty. Each chain picker offers only routes of the matching kind (routes
without candidates fit either), a configured route of the other kind stays
visible and is flagged, and saving role or routing settings is refused while a
chain or route mixes kinds. In routing settings, a candidate's model override
offers only models of the kind the route's other candidates share.

Every GUI config save (providers including deletion, role profile creation,
replacement and deletion, routing and renames, automatic grouping, project
profile selection, sandbox, and self-improvement) uses an explicit layered save
context. The final TOML text is prepared once, checked for file-local structure
and credential safety, then composed by the normal loader at the target's
original layer position. The raw text follows the same per-file migration,
merge order and finalization as a disk reload; legacy `logical_model` and
Oracle/Advisor bindings therefore have the same meaning before and after a
save. All top-level and named profiles, even unselected ones, are checked for
model-kind and image-provider constraints. The checked bytes are the bytes
written atomically (or returned by preview), not a new serialization.

Registrations and chains may live in different files. Deletion, renaming and
reference updates happen before validation. If the final result is invalid,
the original file is unchanged and validation creates no temporary file.
This also means an unrelated save is refused when another, inactive profile
already has a kind/provider mismatch; repair that profile before saving.
Undefined routes retain their existing warning policy rather than making all
saves equivalent to strict loading.

A save target must be a path actually read by the supplied load options. A
missing main file can be created; a new, undiscovered drop-in or a target masked
by a current-format `file_overrides` value is rejected explicitly. No neighboring
configuration directories are inferred from the save path. Non-GUI consumers
can explicitly choose `StandaloneSaveContext`, which treats the file as the only
user layer plus built-in defaults and reads no home, drop-in, env or CLI layers.
The evaluator's preview/apply uses its actual user configuration layers,
including drop-ins. Preview text, effective override diagnostics and commit all
come from the same `ValidatedSave`. After user approval it revalidates the exact
approved bytes against the current layers, so a kind change in a drop-in while
waiting cannot bypass the save checks.

Codex model intent is separate from layer context: `ExplicitModels` requires an
enabled chat default, so a GUI edit changing or removing its last chat model
(including deleting all models) fails before saving. `BuiltinDefaults` explicitly
requests the built-in Codex models/default and requires both input fields empty;
this intent is available in both standalone and layered contexts.
