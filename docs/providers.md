# Claude and Cursor providers

Open **Provider settings** to add a Claude API, Claude subscription, or Cursor
profile. API keys can be read from an environment variable or saved in the
credential store. Subscription profiles use browser sign-in. Use **Refresh
models** after signing in, select the models to enable, choose a default, and
save the profile. Routing settings can then select the profile.

Claude subscription sign-in uses a local callback. If the browser cannot reach
this machine, paste the final redirect URL or authorization code into the login
dialog. Cursor sign-in waits for the browser authorization to finish.

The footer displays each subscription's remaining quota, with reset information
on hover. Claude shows the five-hour and weekly windows advertised by the
account, plus extra usage charges when enabled. Cursor displays its separate
Cursor Models and Other Models pools, on-demand usage when available, and legacy
request counters when the newer summary is unavailable. Missing limits remain
unknown. A failed refresh keeps
the previous snapshot marked stale and does not interrupt chat.

Claude API keys do not provide the Claude Pro/Max subscription quota endpoint.
The normal request usage and cost views remain available for API-key profiles.

## Configuration

Only credential references are written to TOML; OAuth token families are stored
in the credential store and refreshed automatically.

```toml
version = 2

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

Claude supports normal text and tool calls. Explicit reasoning controls are
currently unavailable; OAuth also requires signed thinking replay before it can
support extended thinking. Cursor does not expose temperature, output-token
limits, or service-tier controls.

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
