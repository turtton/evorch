# External MCP servers

evorch is an **MCP tools client**. A project can declare external
[Model Context Protocol](https://modelcontextprotocol.io/) servers; their tools
then appear to agents next to the built-in tools. Only the MCP *tools* capability
is supported (see [Not supported](#not-supported)).

Two things are different from the built-in tools and are worth reading first:

- A **stdio** server is an arbitrary command that evorch starts **outside the
  sandbox**, with your user's permissions. It never runs until you approve it.
- Server definitions are **project files**. Cloning a repository that contains
  `.mcp.json` or `.evorch/mcp.toml` does not start anything: every server that
  can execute code or send a secret waits for your approval in the GUI.

Opening the settings list, saving a definition, or loading a project never starts
a process or opens a connection. Servers are only contacted when a new run
prepares its tools, or when you press a connection test in the GUI (which also
requires approval).

## Where servers are defined

| File | Role |
|---|---|
| `.evorch/mcp.toml` | evorch's own project file. Read and written by evorch (GUI edits go here). |
| `.mcp.json` | Optional, shared with other MCP clients. **Read-only**: evorch imports it and never writes it. |

Both live in the project root (`.evorch/` is the same directory as the rest of the
project configuration). The settings list is in the project dialog: open
**Project settings** and choose **MCP servers…** (see
[The MCP servers dialog](#the-mcp-servers-dialog)).

### Precedence

evorch reads `.mcp.json` first, then `.evorch/mcp.toml`.

- A server ID that appears in both files uses the `.evorch/mcp.toml` definition
  **as a whole**. Fields are never merged, so an override must be complete.
- A TOML entry that is invalid, or disabled, never exposes the `.mcp.json`
  definition below it. evorch does not fall back to a lower definition when an
  override is broken.
- To change an imported server, save a complete override in `.evorch/mcp.toml`
  (the GUI editor does this). To remove the override and use the `.mcp.json`
  definition again, choose **Remove override**.
- To turn an imported server off without touching `.mcp.json`, choose **Disable**
  (shown as **Hide import** when the server already has an override). This writes a
  *tombstone*, `[servers.<id>]` containing only `enabled = false`. Deleting the
  tombstone with **Remove override** brings the import back. "Remove override" and
  "Disable" are different operations: the first goes back to the `.mcp.json`
  definition, the second hides it.
- A tombstone needs no `type` or other fields. Any other `enabled = false` entry
  is a normal, complete definition that is simply switched off.
- If `.evorch/mcp.toml` cannot be parsed at all (or cannot be read, or exceeds the
  limits below), **no MCP server is prepared for new runs of that project**. Agent
  runs themselves continue, and `.mcp.json` is *not* used as a fallback. The
  GUI shows the problem; fix or remove the file.
- Problems inside one server entry (unknown field, bad URL, …) only mark that
  server invalid. The other servers keep working.

### `.evorch/mcp.toml`

```toml
version = 1

[servers.files]
type = "stdio"
enabled = true
roles = ["worker"]
command = "node"
args = ["./tools/mcp-server.js"]
cwd = "."
startup_timeout_ms = 10000
request_timeout_ms = 60000

[servers.files.env]
MODE = "development"
API_TOKEN = "${FILES_API_TOKEN}"
REGION = "${MCP_REGION:-local}"
OTHER_TOKEN = { keychain = "files-token" }

[servers.remote]
type = "http"
enabled = true
roles = ["worker", "web_researcher"]
url = "https://mcp.example.com/mcp"
token = { keychain = "remote-token" }

[servers.remote.headers]
X-Tenant = "${MCP_TENANT:-default}"

[servers.local]
type = "http"
enabled = true
roles = ["worker"]
url = "http://127.0.0.1:3000/mcp"

# Disable a server imported from .mcp.json (a tombstone)
[servers.disabled-import]
enabled = false
```

`version = 1` is required and the only top-level keys are `version` and
`servers`. A server ID is any non-empty text of at most 128 bytes without control
characters; it is used as the TOML table name.

| Field | Applies to | Meaning |
|---|---|---|
| `type` | both | `"stdio"` or `"http"` (Streamable HTTP). Required, except in a tombstone. |
| `enabled` | both | Default `true`. |
| `roles` | both | Agent roles that receive the tools. Default `["worker"]`; `[]` publishes nothing. See [Roles](#roles). |
| `startup_timeout_ms`, `request_timeout_ms` | both | 1 to 600000. Defaults 10000 and 60000. |
| `command` | stdio | Executable. Required, non-empty, no NUL. Not run through a shell. |
| `args` | stdio | Argument list, passed as is (no shell, no globbing, no word splitting). |
| `cwd` | stdio | Working directory, relative to the project root (default: the root). |
| `env` | stdio | Extra environment variables for the process. |
| `url` | http | `http://` or `https://` endpoint. No user info (`user:pass@`) and no `#fragment`. |
| `headers` | http | Extra request headers. |
| `token` | http | Bearer token (sent as `Authorization: Bearer …`). Cannot be combined with an `Authorization` header. |

Rules that save and load both enforce:

- Unknown fields, duplicate keys, unknown role names and fields of the other
  transport (`url` on a stdio server, `command` on an HTTP server) are errors.
  Role names are the agent role keys: `orchestrator`, `explorer`, `worker`,
  `reviewer`, `web_researcher`, `planner`, `advisor`, `multimodal_looker`,
  `evaluator`.
- `env`, `headers` and `token` values are a string or `{ keychain = "name" }`.
  A keychain reference is **not** allowed in `command`, `args`, `cwd` or `url`.
  Keychain names use `A-Z a-z 0-9 . _ -` (at most 128 bytes).
- Environment variable names in `env` must look like `[A-Za-z_][A-Za-z0-9_]*`.
- Header names must be valid HTTP tokens, no duplicates (case-insensitive), no
  CR/LF or other control characters in values. The client owns these headers and
  rejects an attempt to set them: `Host`, `Content-Length`, `Content-Type`,
  `Content-Encoding`, `Accept`, `Transfer-Encoding`, `Connection`, `Keep-Alive`,
  `TE`, `Trailer`, `Upgrade`, `Expect`, `Last-Event-ID`, and anything starting
  with `Mcp-` or `Proxy-`.
- Size limits: configuration files 1 MiB each, 64 servers per project, 64 KiB per
  string, 1024 arguments, 256 entries per `env`/`headers`. Reaching a limit is an
  error, never a silent truncation.
- GUI saves replace the file atomically with mode `0600`, refuse to write
  through a symlink, and refuse to save when either file changed on disk since it
  was loaded (reload, then save again). Comments and unrelated entries in the TOML
  file are kept.

### `.mcp.json`

The format used by other MCP clients:

```json
{
  "mcpServers": {
    "files": {
      "command": "node",
      "args": ["./server.js"],
      "env": { "TOKEN": "${TOKEN}" }
    },
    "remote": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${TOKEN}" }
    }
  }
}
```

- An entry with `command` (or `"type": "stdio"`) is a stdio server;
  `"type": "http"` is a Streamable HTTP server. Any other `type` (for example
  `sse`) and entries that fit neither shape are reported as invalid; evorch does
  not guess a conversion.
- Supported fields: `type`, `command`, `args`, `env`, `cwd`, `url`, `headers`,
  with string values only. Imports are always enabled and publish to the
  `worker` role. To change that, add a complete override in `.evorch/mcp.toml`.
- Other top-level keys in the file are ignored. A file that is not valid JSON, or
  that repeats a key inside `mcpServers`, is reported and **none** of its servers
  are loaded; `.evorch/mcp.toml` is still read.
- Keychain references, `token`, `roles`, `enabled` and timeouts are TOML-only.
  `.mcp.json` cannot express them and is never rewritten to add them.

## The MCP servers dialog

Open **Project settings** and choose **MCP servers…**. The **MCP servers**
dialog opens over the project settings (**Close** leaves it). Opening it, saving
and every button except **Test connection** never start a process or connect to
a server, and the dialog repeats the rule that saved changes apply from the next
new run.

Each row shows the server ID and badges: where it is defined (`.mcp.json`,
`mcp.toml` or `override`), the transport (`stdio` or `http`), `disabled` or
`invalid`, and the latest state reported for it (see
[Troubleshooting](#troubleshooting)), then its roles. Row buttons depend on where
the server is defined:

| Server | Buttons |
|---|---|
| Only in `.mcp.json` | **Edit** (saves a complete override), **Disable** (writes a tombstone) |
| `.mcp.json` entry overridden in `mcp.toml` (`override`) | **Edit**, **Enable** / **Disable** (switches `enabled` of the override), **Hide import** (tombstone), **Remove override** (back to the `.mcp.json` definition) |
| Tombstone over an import | **Remove override** only |
| Only in `mcp.toml` | **Edit**, **Enable** / **Disable**, **Delete** (asks "Delete … from .evorch/mcp.toml?" first) |

For a server that can run, the row also has **Approve…** (shown as **Review
approval** once approved), **Test connection**, and **Revoke approval** (only when
approved). **Test connection** is disabled until the server is approved and runs
the same preparation a run does, so it counts against the same limits. If you
revoke approval, the row notes that conversations already running keep their
tool list but calls to this server are refused.

**Add MCP server** opens the editor: server name, transport (**Local command
(stdio)** or **HTTP**), **Enabled**, a checkbox per agent role, the transport's
fields (command, arguments one per row, working directory relative to the project
root, environment variables; or URL, bearer token, headers), and a **Timeouts**
section. **Save server** always writes the complete definition to
`.evorch/mcp.toml`; editing an imported server saves an override. The editor
warns about a plain-text secret, about no role being selected, and about
`http://`. If either file changed on disk since it was loaded, nothing is saved:
use **Reload** and save again. A broken `.evorch/mcp.toml` is shown as a warning
that no MCP server starts for new runs until it is fixed.

### Notification for skipped servers

When a new run starts without a server because it is not approved, a card appears
in the bottom-right corner (up to three at a time) saying that the MCP server was
skipped, that the run continued, and that approval takes effect from the next
new run. **Review approval** opens the project's MCP servers and the approval
dialog for that server; **Dismiss** closes the card. The card does not appear
while that project's MCP servers dialog is open, because its rows show the same
state.

### Keychain secrets in the editor

For a bearer token, header or environment variable set to **Keychain**, give the
entry a name. The **OS keychain secrets** area then lists each reference with its
state (*stored*, *saved reference, value hidden*, or *no value set here yet*):

- **Set value** / **Replace value** opens a masked input. **Store in keychain and
  save** writes the value to the OS keychain and then saves the server, so the
  form must be valid first. `.evorch/mcp.toml` keeps only the entry name, and the
  value is never shown again. If the keychain cannot store it, nothing is saved
  and evorch does not fall back to a file. If saving the server fails after the
  value was stored (for example because the files changed on disk), the key
  that this operation created is removed again; a key that already existed
  (replacing a value) is never deleted.
- **Delete** (on a reference the saved server uses) first saves the server without
  any reference to the entry, and then deletes the value from the keychain.
  This can succeed only in part: if the keychain cannot be reached (unavailable,
  locked, access denied or cancelled), the server is saved without the reference
  but the value may still be stored in the OS keychain, and the dialog says so.
  Delete it there, or try again later. If the settings cannot be saved at all,
  nothing changes. **Remove** (on a reference that was never saved) only edits the
  form.

## Environment variables

`command`, `args`, `cwd`, `url`, and every value in `env`, `headers` and `token`
accept the same expansion syntax in **both** files. It is expanded from evorch's own
process environment when a run prepares the server.

| Syntax | Result |
|---|---|
| `${VAR}` | Value of `VAR`. If `VAR` is unset, that server is skipped and shown as *invalid* with "An environment variable used in `<field>` is not set in evorch's environment" (`<field>` is `command`, `args`, `cwd`, `env`, `url`, `headers` or `token`; the variable name is not shown). An empty but set `VAR` expands to the empty string. |
| `${VAR:-default}` | Value of `VAR`; `default` if `VAR` is unset **or empty**. |
| `$${VAR}` | The literal text `${VAR}`. |

- Expansion is **not recursive**. An expanded value or a default is never
  scanned again, so a variable whose value contains `${OTHER}` yields that text.
  Defaults are literal text and cannot contain `}` or `${`.
- There is no command substitution and no shell syntax. `$VAR` (without braces),
  a lone `$` and `~` are ordinary text.
- A malformed reference (`${`, `${1X}`, …) makes that server invalid. evorch never
  modifies its own environment.
- A server that evorch reads from a project is expanded from **evorch's**
  environment, not from your shell's if evorch was started some other way (for
  example from a desktop launcher). Set the variable where evorch starts.

The reference names and defaults, not the expanded values, are what approval
tracks (see [Approval](#approval)).

## Secrets

There are three ways to give a server a secret (API key, token). Choose by how much
exposure you accept.

1. **Plain text in the file**, for example `API_TOKEN = "abc123"`. It works, but
   the value is stored as typed in `.evorch/mcp.toml` (or `.mcp.json`) and is
   probably committed or shared with the project. The GUI editor warns when it
   sees this and hides what you type into a plain-text value whose name looks
   like a credential. Use plain text for non-secret values.
2. **`${VAR}`** reads the value from evorch's process environment, so the secret
   is not in the project file. The environment must define it where evorch
   starts. This is a good fit for CI-like and headless use.
3. **OS keychain**: `{ keychain = "remote-token" }` (in `env`, `headers` or
   `token` of the TOML file). The value lives in Linux Secret Service or the
   macOS Keychain, and the file stores only the name. In the GUI editor you
   choose **Keychain** for the bearer token, a header or an environment variable,
   name the entry, and set its value there (see
   [Keychain secrets in the editor](#keychain-secrets-in-the-editor)). Updating
   the value under the same name does not change the TOML.

Details:

- evorch uses a separate MCP credential namespace derived from the project, the
  server ID and the name. MCP secret references cannot address provider
  credential entries, and the same name in two projects or servers identifies
  two different entries. This namespace only limits the secret resolver: it
  does not restrict a stdio program's access to the host with your permissions.
- If the OS keychain is unavailable, locked or denied, **evorch does not fall back
  to a file or environment variable**. The server is reported as *Secret
  unavailable* and is skipped; servers that do not use the keychain are
  unaffected.
- Secret values are never shown in the GUI, logs, events, transcripts or the
  approval record.

> **Do not write secrets into `args` or into a URL query** (for example
> `https://example.com/mcp?api_key=…`). The approval dialog shows arguments and
> the URL, including the query, as plain text so that you can judge what is
> approved, and they are stored in the project file. Pass the secret through
> `env`, a header or `token` instead, as `${VAR}` or a keychain entry.

On the approval screen evorch hides some plain-text values; see
[Approving in the GUI](#approving-in-the-gui). That is a display safeguard only.
A value written as plain text stays plain text in the file.

## stdio servers run outside the sandbox

A stdio server is started directly (no shell) as a child process of evorch, with
your user's privileges, **outside** the sandbox that confines the agent's `shell`
tool. It can read and write anything you can. Only approve servers whose command
you trust, and re-read the approval preview when it changes.

The process is started with a minimal environment: evorch clears the inherited
environment, copies only the variables below when they exist, and then applies the
`env` entries of the definition:

`HOME`, `PATH`, `USER`, `LOGNAME`, `TMPDIR`, `LANG`, `LANGUAGE`, `LC_ALL`,
`LC_CTYPE`, `LC_COLLATE`, `LC_MESSAGES`, `LC_MONETARY`, `LC_NUMERIC`, `LC_TIME`,
`LC_ADDRESS`, `LC_IDENTIFICATION`, `LC_MEASUREMENT`, `LC_NAME`, `LC_PAPER`,
`LC_TELEPHONE`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`,
`XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `XDG_CONFIG_DIRS`, `XDG_DATA_DIRS`.

Nothing else is inherited: not `SSH_*`, `DBUS_*`, proxy settings, cloud or
provider tokens, `NODE_OPTIONS`, `PYTHONPATH`, `LD_*`, and so on. If a server needs
one, name it explicitly in `env`, for example `HTTPS_PROXY = "${HTTPS_PROXY}"`.
If a command fails to start, evorch's log lists the *names* (never values) of
environment variables that are set but not inherited, as a hint. It is context,
not a diagnosis of the cause, and it is not shown in the GUI.

How the program is found: `command` is expanded first. If it contains a path
separator it is taken relative to the working directory; otherwise each `PATH`
entry is tried in order (a relative `PATH` entry is taken relative to the working
directory). The match must be an executable regular file, and its symlink-resolved
path is the **resolved program** that you approve. If no executable is found the
server is *unavailable*.

The working directory (`cwd`, default `.`) is joined to the project root and
resolved to a canonical directory that must exist. It is **not** required to stay
inside the project: an absolute path or `..` is accepted. The approval dialog
shows the resulting canonical path, so check it.

A launched server is supervised: it is started in its own process group,
`stdout` carries one JSON-RPC message per line, `stderr` is drained and not
shown as chat output, and when a run ends or is cancelled evorch closes its
stdin, then sends SIGTERM and finally SIGKILL (with short grace periods) and reaps
the whole group. A server is restarted at most once per run. A call whose result
is unknown after a failure is **not** resent (see
[Failures and recovery](#failures-and-recovery)).

At most **4 stdio processes** run at the same time **per project**. The limit is
shared by all runs, child runs, GUI connection tests and restored sessions.
Beyond it, the extra server is skipped (status *process limit*) instead of
waiting.

## Approval

Starting a process or sending a secret needs your explicit approval in the GUI.
Approval is separate from per-call tool permissions: it decides whether a server may
start or connect, while each tool call still goes through the normal agent role,
scope and permission checks. Project trust, a flag in a config file, or the model
answering a question never count as approval, and approval actions are not
available as an agent tool.

### Which servers need approval

| Server | Approval |
|---|---|
| stdio | Always. |
| HTTP to `localhost` or a LAN address (loopback, `10/8`, `172.16/12`, `192.168/16`, IPv6 unique-local) | Always, bound to the IP addresses seen at approval. |
| HTTP with `http://` (plain text) | Always; only allowed for localhost/LAN, see [HTTP](#http-connections). |
| HTTPS to a public host that sends an `${VAR}` or keychain value (in `url`, `headers`, `token`) | On first use, and again when the destination or a reference name changes. |
| HTTPS to a public host with only literal header/token values and no references | No GUI approval; the network checks still apply. Literal values are plain text; prefer a reference for anything secret. |

### What the approval is bound to

The approval is stored as a SHA-256 fingerprint of a canonical *projection* of the
launch or connection definition, for one project root and one server ID. It is
**not** a hash of the whole config file, so unrelated edits (other servers,
comments, roles, timeouts, `enabled`) do not ask again. Approval is needed again when
any of these change:

- **stdio**: `command`, the **resolved executable path**, `args`, the
  canonical `cwd`, and `env` as defined: key names, literal values, and for
  `${VAR}` references and keychain references the reference *names and
  defaults*. Switching a value between literal and `${VAR}`/keychain is a
  change. If a `PATH` change makes the same `command` resolve to a different
  executable, approval is needed again.
- **HTTP**: the scheme, host and port of the resolved URL are always part of the
  approval, even when a reference supplies them. The URL path and query are
  included as literal text or, where a `${VAR}` supplies part of them, as a
  symbolic form with the reference name and default. The names of the
  referenced environment variables and keychain entries are included, as are the
  header names. Literal header and token values are *not* included, only whether a
  value is a literal.
- **localhost/LAN HTTP**: all of the above plus the IP addresses that the host
  resolved to when you approved. A new private IP address for the same name,
  or a different destination, needs a new approval.

Not part of the approval (changing them never asks again): the **value** of a
secret behind an unchanged reference (rotate secrets freely), the values of
inherited variables such as `PATH`, the `roles`, timeouts and `enabled` flags, and
connection state or tool lists. Using a reference with the same name keeps
approval; adding a new reference name does not.

Revoking approval also stops sessions that are already running: their tools stay
listed in the conversation but later calls are refused. Approving again restores
them only for new runs.

### Where approvals are stored

`<user_config_dir>/mcp-approvals.json`, where `<user_config_dir>` is the same
per-user evorch directory as the user configuration (`$XDG_CONFIG_HOME/evorch`,
otherwise `~/.config/evorch`). The directory is created with mode `0700` and
the file `0600`, written atomically under a lock. For each project root and
server ID it holds the fingerprint, an approved/revoked flag, the approved IP
addresses of a local destination, and a safe summary that the approval dialog
uses to show what changed: the resolved program path, the canonical working
directory, the destination scheme, host and port, environment variable names,
and secret reference names. It never holds literal values, expanded values,
arguments, URL paths or queries, header values or secrets.

- If the user configuration directory cannot be determined, approvals cannot be
  saved. evorch does **not** use the project directory instead.
- If the file cannot be read, is damaged, or is readable by group or others, all
  servers are treated as **not approved** until fixed.

### Approving in the GUI

A server that needs approval shows **Approve…** (or **Review approval** when it
was approved before) in its row; the notification card has the same entry point.
The **Approve MCP server** dialog shows exactly what would be allowed:

- *What runs* (stdio): a warning that it starts a program outside the sandbox
  with your user permissions, the **Command**, the **Resolved program**, each
  argument, the canonical **Working directory**, and the environment variables
  (`${VAR}` and keychain references by name, never resolved). A note says that
  besides these only basic variables such as `HOME`, `PATH`, `USER`, `LANG` and
  `XDG_*` are passed.
- *Where it connects* (HTTP): the **URL** (including any query), the
  **Destination** (`scheme://host:port`), the **Bearer token**, the **Headers**,
  and for a localhost/LAN server the **Allowed addresses**. Showing an HTTP
  preview resolves the host name with DNS to record these addresses; it sends
  nothing to the server.
- **Secrets used**, the names of the referenced environment variables and
  keychain entries, and warnings: runs outside the sandbox, local-network
  address (only the listed addresses are used), `http://` is not encrypted, and
  which secrets are sent with every request.
- A note that approval allows exactly these settings and does not check what the
  program or server does.

Arguments, the URL and plain values are shown as written. Two kinds of plain text
are hidden, shown as `[hidden text, N characters]`: a literal bearer token, and
the literal value of any environment variable or header whose name contains
`token`, `secret`, `pass`, `key`, `auth`, `credential`, `cookie` or `session`
(case-insensitive, so `Authorization` is covered). Other values are plain, which is
why secrets do not belong in `args` or in a URL query.

If the server was approved before, the dialog says what changed. Only the
recorded summary is kept from the earlier approval, so it can list a changed
program, working directory, destination, environment variable name or secret
reference name (*Added* / *Removed* / *Changed*). A change to anything else
(command, arguments, URL, plain values, allowed addresses) cannot be shown in
detail: the dialog then says other settings may have changed, that their previous
values are not kept, and asks you to check everything. *Already approved with
exactly these settings* is shown when nothing differs.

A preview is tied to the exact files and fingerprint you saw. If anything changed
on disk or the definition changed since, **Approve** is refused as stale
(*Review again* reopens it). After approval the row says it is used from the next
new run. **Revoke approval** removes it again. **Test connection** needs the same
approval and otherwise reports that nothing was started or contacted.

## HTTP connections

Streamable HTTP servers go through evorch's network guard, with MCP-specific rules:

- **Public servers use HTTPS only.** `http://` is accepted only for an approved
  localhost or LAN destination. Plain HTTP to a public address, or a name that
  resolves to a mix of public and local addresses, is refused.
- Link-local (including cloud metadata addresses), carrier-grade NAT,
  multicast, broadcast and unspecified addresses are always refused. There is no
  way to allow them.
- DNS answers are checked and **pinned**: the connection goes to the checked
  addresses only, and for an approved local server again to the addresses you
  approved. TLS certificates are always verified. System proxy settings are not
  used.
- **Redirects are refused.** Put the final URL in the configuration.
- The localhost/LAN exception covers only the approved server and address. It
  never extends to `web_fetch`, web search or another MCP server.
- Request and response sizes and the number of frames per exchange are bounded.
- Sending a secret over plain `http://` is possible on a trusted LAN (the
  approval preview says so) but anyone on the network path can read it.

Plain-text header values are sent as written; they need no confirmation but are
visible to anyone who can read the project files.

### Failures and recovery

- **Modern HTTP** is stateless. A call that fails (connection error, timeout or
  unknown outcome) is never sent again by evorch. A *later*, separate call to
  the same server is allowed: evorch first lists the server's tools again and
  proceeds if the tools this conversation started with are still there with the
  same definitions, otherwise that call is refused as "the server's tool
  definitions changed since this conversation started".
- **Legacy HTTP** keeps a server session. After a failure, evorch opens a
  replacement session at most **once per run and server**, checks the tool
  definitions the same way, and does not repeat the failed call.
- **stdio** restarts the process at most **once per run and server**, with the
  same check, and does not repeat the failed call.
- After that single recovery is used up, further calls to the server in this
  run fail as unavailable. A new run starts fresh.
- Recovery has its own 30-second budget, and at most **4 HTTP connections per
  project** are being prepared (new runs, recovery, connection tests) at the same
  time. Further ones are not queued: the server is *unavailable* for that run with
  "Too many MCP connections are being prepared for this project; this server is
  unavailable for this run." This is unrelated to response size limits.

## Skipped servers and when changes take effect

- A server that is **not approved** is **skipped**: it is not started, not
  contacted (no discovery, no secret read), and its tools are not offered. The
  run continues without it, a warning is recorded for the run, and the GUI shows
  the notification card with **Review approval**. The same applies to servers that
  are invalid, unavailable, over the process limit, or whose secrets cannot be
  resolved; the other servers are unaffected. A server whose `roles` do not
  include the run's role is not prepared at all and does not appear in that run's
  tool list or in the state shown for the project (a run of another role cannot
  turn a `ready` server into `off`).
- New runs prepare the servers of the project one at a time in server-ID order
  within a 30-second budget for the whole preparation. A server that runs past
  the budget is skipped as unavailable, and so are the ones after it.
- If `.evorch/mcp.toml` cannot be read, no server is prepared and a warning is
  recorded; the run continues without MCP tools. Benchmark runs never prepare
  MCP servers.
- **Approving, editing the configuration, or saving a secret affects new runs.**
  A tool list is fixed when a run starts. A conversation that is already running
  keeps its tools, even if the server is approved afterwards, edited,
  recovers from an outage, or announces a list change. This keeps the
  provider prompt prefix stable (prompt cache). Start a new run to pick up changes.
- Each new run fetches the tool list again; lists are not cached between runs.
- A continued or restored conversation keeps the tool list it started with.
  Restoring does not start any server and does not read the configuration or
  secrets. A server is contacted on the first call to its tools.
- **Every call** re-checks, against the current files, that the server is still
  enabled, still published to the run's role, and still has the same approval
  fingerprint (including, for a local HTTP server, the same addresses). Calls to
  that server are refused, while its tools stay listed, when:
  - you disabled the server, revoked approval, or edited something the approval
    covers (command, arguments, URL, environment, …) after the conversation
    started: "Approval was revoked; calls to this server are refused."
  - you removed the run's role from `roles`: "This server is not shared with
    this role; calls to it are refused."

  Rotating a secret value triggers neither. When a restored or lazily connected
  server is contacted for the first time, the tool being called must also still
  have the definition that was saved; otherwise that call is refused.

## Roles

`roles` lists the agent roles whose runs receive the server's tools. The default
is `["worker"]`; `[]` publishes the server to nobody and connection tests return
*unavailable* rather than *off*, without starting or contacting it. Roles that
are not listed neither see the tools nor cause the server to be started. A role
change does not need re-approval, but removing the run's role from `roles` stops calls in
conversations that are already running ("This server is not shared with this
role"; see [Skipped servers](#skipped-servers-and-when-changes-take-effect)). The tools
cannot be called from `run_js`.

## Tool names

An external tool is published to the model as `mcp__<server>__<tool>`, for
example `mcp__files__read_file`.

Names are never altered to make them fit. If the server ID or the tool name
contains anything other than `A-Z a-z 0-9 _ -`, if the name would exceed 64
characters, or if two tools would receive the same name, a hashed name
`mcp_h__<base64url SHA-256>` is used instead. The hash covers the server ID and the
tool name together, and in a name collision both tools are hashed, so names
do not depend on discovery order. The name is only a label: evorch looks the
real server and tool up in a table and never parses the name. The tool's
description and schema are those of the server.

The names the model *sees* can differ from these for some providers (see
[Claude and Cursor providers](providers.md)):

| Where | Name |
|---|---|
| evorch internally, API-key profiles | `mcp__files__read_file` / `mcp_h__<hash>` |
| Claude subscription wire | `_mcp__evorch__mcp__files__read_file` / `_mcp__evorch__mcp_h__<hash>` |

The `_mcp__evorch__` prefix and `_mcp__evorch_h__` hash used by that provider are a
separate, older presentation layer for evorch's own tools. External names are an
extra, independent layer inside it; the provider layer is undone before execution.

## Protocol versions

evorch speaks two eras of the protocol and picks one per connection:

- **2026-07-28** (modern): the connection starts with `server/discover`; every
  request declares its protocol version.
- **Legacy `initialize` versions**: `2025-11-25`, `2025-06-18`, `2025-03-26`, and
  `2024-11-05` for stdio only. The client sends `initialize` and then
  `notifications/initialized`, and uses the version the server selects.

Era detection: for stdio, a successful `server/discover` or a recognisable
modern error means modern; any other protocol error, or no answer in time,
means legacy. For HTTP, an HTTP 400 whose body is a modern version error stays
modern, and other 400 answers mean legacy. Authentication failures (401, 403),
DNS and TLS refusals are reported and never treated as a reason to downgrade.
`tools/call` is never used to probe, and a call whose outcome is unknown is not
retried.

Tool lists are limited to 100 pages, 1000 tools and 8 MiB per server. A tool
whose definition or JSON Schema is invalid (or whose HTTP header annotation is
invalid) is left out and the others are kept; duplicate tool names or a broken list
make the whole server unavailable.

## Tool results

A tool call's result reaches the model as follows:

- Text content is returned as text, in order, and `structuredContent` is appended
  as JSON.
- Images are attached (up to the per-result image limit; an image that cannot be
  decoded or is too large is replaced by a short note).
- Audio and embedded resources (text or binary) are not placed in the model's
  context as base64. They are saved as local artifact files (at most 8 per result)
  and described, for example `[audio mimeType="audio/wav"; playback unsupported;
  1234 bytes; artifact: <path>]`, or `[embedded resource uri=… mimeType=…; …; URI
  not fetched]`. If saving fails, the note says `artifact unavailable`.
- A `resource_link` is a reference only: its name and URI (each bounded to 512
  bytes) are shown as `[resource_link …; reference only; not fetched]`. evorch
  never fetches the URI.
- The whole result is then bounded like any other tool output; an oversized
  result becomes an artifact reference once, and earlier results are not
  rewritten.
- `isError: true` in a result is reported as a tool error and kept distinct from
  a connection failure. A server asking for interactive input (`input_required`)
  is a failure, not a result.

## Not supported

evorch is an MCP tools client. Not supported:

- OAuth or any other interactive authorization flow; use a static token.
- Multi round-trip requests (MRTR): a server asking for more input is detected,
  the call fails with an error saying that interactive continuation is
  unsupported, and it is not re-run.
- Resources and prompts: they are not listed or offered to the model, and URIs
  in tool results are never fetched (see [Tool results](#tool-results)).
- Subscriptions and list-change notifications (`listen`): the tool list is never
  updated during a run.
- The 2024 style HTTP+SSE transport (a separate SSE endpoint). Use Streamable
  HTTP.
- A user-wide MCP configuration. Servers are defined per project only.

## Troubleshooting

The server list in the GUI shows a state badge for each server. Messages show only
safe diagnostics: the server ID, the operation and a failure class, never URLs,
headers, arguments, environment values, server output or secrets. The hover text
carries the same safe record. The badge is the latest state reported for the
project by a run or a connection test.

| Badge | Meaning and what to do |
|---|---|
| `off` | The server is disabled (`enabled = false`) or a tombstone. Enable it, or choose **Remove override** to use the `.mcp.json` definition again. A run whose role is not in `roles` does not change this badge. |
| `invalid` | The definition does not validate (unknown field, bad URL, a header that evorch manages, malformed `${…}` syntax, limit exceeded…), a `${VAR}` it uses is not set in evorch's environment ("An environment variable used in … is not set"), or `.evorch/mcp.toml` cannot be read at all. Fix the entry or set the variable. |
| `needs approval` | Not started: it is not approved (or approval was revoked). Choose **Approve…**, read the dialog, approve. It applies from the next new run. |
| `secret unavailable` | A keychain entry is missing ("has no stored value yet"), or the OS keychain is not available, locked, or access was denied or cancelled. Set the value in the editor, unlock the keychain, or try again. evorch never falls back to a file. |
| `connecting` | A connection or test is in progress. |
| `ready` | The server connected and its tools are available to new runs. |
| `unavailable` | The server was not reached or failed: the program was not found or could not start (check `command`, `PATH` and the minimal environment), the working directory does not exist, the endpoint refused, timed out or answered with an HTTP error or invalid MCP, the 30-second preparation budget was used up, or too many HTTP connections were being prepared ("Too many MCP connections are being prepared for this project; this server is unavailable for this run."). An enabled server with `roles = []` is also `unavailable`: "This server is not shared with this role; calls to it are refused." The message states the class. |
| `process limit` | The project already has 4 stdio processes. Finish or cancel other runs. |

Common problems:

- **Tools are missing in the conversation I am in.** Tool lists are fixed per
  run. Start a new run after approving or editing a server.
- **Command not found, but it works in my terminal.** The process gets a minimal
  environment. Use an absolute `command`, or set the needed variable in `env`.
- **Approval is requested again.** Something in the approval projection changed:
  `command`, the resolved executable (for example `PATH` changed), `args`, `cwd`, an
  environment entry or reference name, the URL or destination, or the IP
  addresses of a local server. Secret values, roles and timeouts do not.
- **An HTTP server is refused.** Use HTTPS for public hosts, remove redirects,
  and for a local server approve its current address.
- **Calls fail with "Approval was revoked" in a running conversation.** The
  server was disabled, approval was revoked, or an approved setting was edited
  after the conversation started. Start a new run (and approve again if needed).
- **Calls fail with "This server is not shared with this role".** The run's role
  was removed from the server's `roles` after the conversation started. Add the
  role back, or start a new run with a role that is listed.
- **The keychain is unavailable.** Servers using it are skipped. Plaintext or
  `${VAR}` values still work, because evorch does not substitute a file store.
