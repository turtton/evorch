# Claude and Cursor providers

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
list; the selected variant determines these settings.

Claude supports text and tool calls, and preserves signed thinking returned by
the provider across tool continuations and saved history. Subscription profiles
support model-validated reasoning effort, including adaptive thinking on
Sonnet 4.6 and Opus 4.6/4.7; unsupported model/effort combinations fail before
inference. Agent Roles and the chat model picker offer the effort levels
supported by the selected subscription model. Models that require thinking
do not offer `none`, and unrecognized models have no effort selector.
See the [Anthropic stream contract](anthropic-stream-events.md)
for supported events and signature replay boundaries. Cursor does not expose
temperature, output-token limits, or service-tier controls.

The protocol implementation was informed by
[oh-my-pi](https://github.com/can1357/oh-my-pi/tree/602b6c812fa9ef774f359f1e399a09d30ee2eaca),
including its OAuth, native Cursor transport, and quota parsers. Offline mock
tests verify authentication, token rotation, tool continuations, and prompt
prefix preservation. Live account acceptance requires an actual account.

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
  re-summarized from the raw messages it covers when the new member cannot
  replay it. The blob itself is never read or forwarded.
- The history is compacted when it is above the new member's context window
  × `[compaction].threshold`.

Both happen once per switch, then the request is sent again; a restored run
whose pinned member is unavailable goes through the same steps.

Summaries use official compaction when the run's member supports it, then the
chain, then the structural summary, which needs no model and always completes.
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
not use yet.

### Output limits

For Anthropic Messages and OpenAI Chat Completions, each request resolves its
output limit from explicit role/category `generation.max_tokens`, the selected
model's preset `max_output_tokens`, then models.dev. Positive metadata values
are used; unknown limits retain the provider's default. The selected profile
and model are resolved again on fallback. A `+fast` variant uses its exact
configured preset when present and its base model for catalog lookup.
Codex and Cursor do not automatically receive this limit because their wire
formats do not support that generation field.
Requests to the official OpenAI HTTPS endpoint (`https://api.openai.com`, port
443), including configured `openai-compatible` profiles, send this limit as
`max_completion_tokens`, including reasoning tokens. Other compatible endpoints
and Kimi retain `max_tokens`.

### Automatic pools

Saving a provider in the GUI adds its models to the pools that serve the same
model. A model's identity is its models.dev `canonical_model_id` together with
its tier: `+fast`, or a `fast`, `highspeed`, `flex`, `eco` or `free` segment of
the provider's model ID. Pools never mix tiers. The catalog entry is found
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
