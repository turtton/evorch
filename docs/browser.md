# Embedded Browser

The agent browser (launch, CDP, network guard) lives in `crates/browser` so
runtime and tools can use it. The GUI tab that watches it is behind the
`browser` feature of `gui`, which is on by default; build with
`--no-default-features` to leave it out. CEF builds include it.
Install Chromium separately. No browser is downloaded or opened automatically.

The GUI used to have a manual **Browser** window (navigate a URL, click a CSS
selector and record `browser.action`, `browser.screenshot` and
`browser.dom_diff` diagnostics). It predated the agent tools and the CEF
preview and was removed; presented HTML opens in the in-app preview (below),
and agent pages show in the agent browser tab. The diagnostic codes stay
defined so that events stored by older versions still read.

The agent browser keeps memory bounded: screencasts are limited to 1280x720
and decoded with image allocation limits, and only the latest decoded frame of
the watched page is kept.

## Agent browser tools

When a Chromium or Chrome executable is installed (or named by `CHROME`), the
runtime registers `browser_navigate`, `browser_navigate_back`,
`browser_navigate_forward`, `browser_snapshot`, `browser_click`,
`browser_hover`, `browser_type`, `browser_select_option`, `browser_upload_file`,
`browser_press_key`, `browser_scroll`, `browser_wait_for`, `browser_handle_dialog`,
`browser_tabs`, `browser_switch_tab`, `browser_close_tab`,
`browser_screenshot` and `browser_close` (ADR 0034). WebResearcher and the conversation root Worker may
use them; `sandbox.web_tools_enabled` is their global switch, as for the other
Web tools.

`[tools.browser]` in the user configuration (`~/.config/evorch/config.toml`
or its `config.d/`) adjusts them. Project files, environment variables and CLI
overrides cannot set it, because it names a program to run and widens what
agents can do.

```toml
[tools.browser]
enabled = true                    # false keeps the tools unregistered
executable = "/usr/bin/chromium"  # default: CHROME, then auto-detection
max_pages = 8                     # tabs across all runs; more closes the least recently used run
max_tabs = 4                      # tabs per run; a popup beyond this is closed
coordinate_actions = false        # true also registers browser_click_at
```

`browser_click_at` clicks a point given in CSS pixels from the top-left of the
visible viewport, as `browser_screenshot` shows it. It is for targets the
outline cannot name, such as a canvas; refs stay the default.

- One headless Chromium is launched on first use, with a temporary profile.
  Each run gets its own tab; at most `max_pages` tabs stay open across all
  runs, and opening one more closes the least recently used other run with
  all its tabs. A run's tabs close when the run ends or on `browser_close`.
- Links with `target=_blank` and `window.open` open a new tab in the run of
  the page that opened it, `noopener` ones included. The result of the action
  that opened it says so under "Tabs:"; the opener stays the active tab until
  `browser_switch_tab`, and every other tool acts on the active tab.
  `browser_tabs` lists the run's tabs (`t1`, `t2`, ...; ids are not reused)
  with their URL, title and opener. `browser_close_tab` closes one; the tab
  that opened it, or else the newest one left, becomes active.
- A run holds at most `min(max_tabs, max_pages)` tabs. A popup beyond that is
  closed as soon as it is caught (until then its requests pass the same
  checks as any other tab), and the next result says so. A page that
  no run opened, such as one a page could create some other way, is closed.
- Pages are read as an accessibility outline. Elements carry `[ref=eN]`,
  where `N` is the element's DOM node, so a ref stays the same for as long as
  the element exists. The action tools accept any ref of the page as last
  read, including refs that the result left out. `browser_screenshot` returns
  a PNG image result.
- `browser_snapshot` returns the whole outline. With `ref` it returns only
  the part under that element, and with `depth` at most that many levels
  below it. A cut level is summed up in one line ("… 12 more lines
  deeper"), so the agent knows where to look closer.
- The other tools return only what changed in the outline since the page
  was last read. Each change is shown as `+` added, `-` removed or `~`
  changed (with the old line), under the unchanged lines that hold it. Lines
  are matched by their DOM node.
  - The whole outline comes back after `browser_navigate`, after
    `browser_switch_tab`, the first time a tab is read, and when the change
    list would be at least half as long as the outline (a new document, for
    instance). With no change, the result says so in one line.
  - Changes are the default because a full outline after every click
    repeats the whole page, up to 800 lines, while the agent needs to see
    what its action did. The change list says that directly, and it costs
    a few lines. The agent loses nothing by it: refs it does not show keep
    working, and `browser_snapshot` reads the whole page at any time.
- `browser_navigate` URLs go through the same per-call review as `web_fetch`.
  Back and forward revisit history entries, which load under the same
  request policy; the back/forward cache is disabled, so every entry loads
  again.
- `browser_wait_for` waits until the active tab shows what the agent names:
  text appearing or disappearing in the outline's names and values, a ref
  appearing or disappearing, no request in flight, the URL containing some
  text, or the URL changing. All named conditions must hold at the same
  check, and the result is the outline.
  - The conditions are checked once, then again after each page event. Page
    events are DOM changes, requests starting or ending, and main-frame loads
    and navigations. A `MutationObserver` reports the DOM changes from an
    isolated world, so the page can neither see it nor send reports through
    it. The Network domain tracks requests in flight.
  - The wait gives up after 1000 page events, which ends it on a busy page.
    A page that does nothing sends no event to count, so the wait also gives
    up after 30 seconds. That is the CDP request timeout the agent browser
    already uses, so a wait blocks the run no longer than one stuck CDP
    command could. A wait that gave up is an error result that still
    carries the outline, and the agent may wait again.
  - Network idle means no request in flight at a check, not a quiet period.
    Long-lived requests such as EventSource streams keep it from holding,
    and WebSockets do not count.
- `browser_select_option` picks `<select>` options by value or visible label
  and fires `input` and `change`, as a selection by the user would.
- `browser_upload_file` sets the files of a file input through
  `DOM.setFileInputFiles`. Only files the run's sandbox may read are accepted:
  the same boundary as `read`, judged after links are resolved, with no
  `unsandboxed` escape, and no uploads at all from an executor without a
  sandbox boundary. Files over 32 MiB and files the secret guard recognises as
  credentials (key blocks, API key shapes, configured credential values) are
  refused; other secrets such as an arbitrary `.env` are only kept out by the
  boundary. Every upload is also reviewed, like a `browser_navigate` URL, by
  the quick reviewer model: it sees the origin and URL of the document that
  holds the file input (which receives the files), the page URL, and each
  file's path (relative to the workspace when inside it) and size, and judges
  whether handing these files to this site serves the user's request. Without
  an approving verdict, including when review is unavailable, nothing is
  uploaded. Chromium, outside the sandbox, reads the accepted files itself,
  and the page can send them only through the proxy and Fetch rules above.
- JavaScript dialogs never wait for an answer. Alerts are closed and
  `beforeunload` lets the page go. Confirm and prompt dialogs are dismissed
  unless `browser_handle_dialog` answered them for the action that opened
  them, so nothing a page guards behind a confirmation runs unless the agent
  asked. Each dialog is listed in the next result. An open dialog would block
  the page and every further tool call on it, which is why none is held open.
- Chromium reaches the network only through a loopback proxy that evorch
  starts with it (`--proxy-server`, with `--proxy-bypass-list=<-loopback>` so
  loopback is not bypassed). The proxy applies the `web_fetch` rules to every
  CONNECT tunnel and plain HTTP request: only TLS tunnels pass, link-local and
  CGNAT addresses are refused, and the proxy dials exactly the addresses it
  checked, so DNS rebinding cannot swap them. WebSockets and worker or
  service-worker fetches take the same path.
- Requests the CDP Fetch domain can pause (navigations, redirects,
  subresources and frames) are also checked there first, and refused ones are
  listed in the next result.
- New tabs are caught before they run anything. A second CDP connection asks
  the browser to auto-attach to every new page and hold it
  (`Target.setAutoAttach` with `waitForDebuggerOnStart` and `flatten`). Fetch
  interception is enabled on the held tab, and only then does it resume, so a
  popup's first request is checked like any other, and it goes through the
  same proxy.
- QUIC is disabled and WebRTC is limited to proxied UDP. TLS certificate
  errors fail the load. Pages see the window's own size (1280 wide): evorch
  does not emulate a viewport.

### Watching the agent browser

In GUI builds with the `browser` feature, a read-only **Browser** tab shows
the agent pages of the open conversation. Every executor of the process,
including the ones runs build for their own sandbox, shares one agent browser.

- When a run of a conversation opens its first page, the tab appears in that
  conversation's subagent region (beside the Subagents tab) and comes to the
  front of it. Keyboard focus stays where it was.
- The tab belongs to its conversation: opening another conversation takes it
  out of the dock, and coming back puts it back, in front only if it was in
  front when the conversation was left.
- It shows the run that last used a browser tool. When several runs of the
  conversation opened pages, a Run picker switches between them.
- Each `browser_*` tool card has a **View** button that shows that run's page
  in the tab, opening the tab again if it was closed. Closing the tab hides it
  until a run of the conversation opens another page or View is used.
- Only the shown run's active tab is screencast, and only while the Browser
  tab is drawn; a tab behind another one or out of the dock stops it. When the
  run switches or closes tabs, the view follows its active tab and names it
  above the frame.
- A page that closed keeps its last frame with "The page was closed." The last
  frames of up to eight runs are kept. Pages do not survive a restart, so a
  saved layout does not bring the tab back.

The latest actions (every browser tool call, with failures), refused requests
and closed dialogs are laid over the frame. Typed text is
shown up to 40 characters. The tab never sends input to the page.

## Low-memory verification

Use `CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

- `cargo test -p gui --test browser`
- `cargo test -p gui --test agent_browser_workbench`
- `cargo test -p browser`
- With installed Chromium: `cargo test -p browser --test agent_chromium -- --ignored`

The Chromium integration test uses only an ephemeral localhost HTTP fixture.
It is ignored by the normal test run to avoid launching Chromium implicitly.
See [gui-verification.md](gui-verification.md) for the full GUI verification
layer matrix, where this test is layer L6.

## In-app HTML preview (CEF)

The `cef` feature of `gui` opens presented HTML artifacts (`.html`/`.htm`) in
an in-app window rendered off-screen by CEF (`crates/browser-cef`, ADR 0034 B).
Other artifacts, and every artifact when CEF cannot start, still go to the XDG
Portal. The Nix default package includes it on x86_64-linux (see Building).
The cargo feature stays off by default, so cargo builds without it never fetch
CEF.

The preview is offline. The artifact is served from
`https://artifact.evorch.invalid` by a scheme handler, every other request and
navigation is cancelled, a Content-Security-Policy is attached, the process
proxy points at an unreachable address, and WebRTC constructors are deleted in
each renderer. A crashed renderer is reloaded up to three times.

What the user can do in the preview pane:

- Native popups: `<select>` dropdowns and other popup widgets are painted
  over the pane (kept inside it) and take pointer input.
- JS dialogs: `alert`, `confirm`, `prompt` and `beforeunload` open an egui
  modal, and the page receives the answer. Closing the modal is Cancel (OK
  for an alert).
- Copying: copies the user makes (Ctrl+C or Ctrl+X on a selection,
  `document.execCommand("copy")`, and `navigator.clipboard.writeText`, each
  only with user activation) reach the host clipboard. Headless CEF keeps its
  own clipboard, so a hook installed before page scripts reports the copied
  text to the browser process. `navigator.clipboard.write` with
  `ClipboardItem` is not forwarded.
- The page never reads the host clipboard: `readText` and `read` reject with
  `NotAllowedError`, and host pastes are not forwarded.
- Still suppressed: file choosers, `window.open` popups and the context menu.

In practice Chromium did not ask for a `beforeunload` dialog in this off-screen
setup, whether the page or the browser reloaded, so the page leaves without a
prompt. The modal is wired for when CEF does ask.

Conversation cards show a snapshot of each HTML artifact. A throwaway browser
renders it offline at 1024×768 DIP under the same isolation, waits for loading
to finish and one more paint, and stores a PNG in
`$XDG_CACHE_HOME/evorch/html-snapshots/<sha256>-v1.png`. Artifacts are
content-addressed, so a cached snapshot is never stale. Snapshots render one at
a time on a worker thread; clicking one opens the in-app preview. Without the
feature, cards keep the text entry and the Open button that goes to the Portal.

Agents check their HTML artifacts through CEF with `inspect_artifact`. The
action `open` loads an artifact of the caller's conversation into an offline
page that no window shows (1024×768 DIP, same isolation). The page is driven by
the same `PageAgent` layer as the `browser_*` tools, over CEF's in-process
DevTools. The actions `snapshot`, `click`, `hover`, `select_option`, `type`,
`press_key`, `scroll` and `screenshot` work as their `browser_*` counterparts.
There is no `navigate`, `back` or `forward`: the tool takes no URL and the page
cannot navigate, so nothing leaves CEF. Refused navigations and
popups are listed in the next outline. Nobody sees the page's JS dialogs:
`alert` is closed, `confirm` and `prompt` are dismissed (`false` and `null`),
`beforeunload` leaves, and the next outline lists each under "Dialogs:".

- Who can use it: visual Workers, and the conversation's root Worker or root
  Orchestrator, the same agents that capture or present artifacts. Ownership
  is checked against the artifact store as for `present`.
- Pages: each run has one page, and at most four are open at once. A run's
  page closes when the run ends or on `close`.
- Startup: CEF is not started with the GUI. At startup evorch only checks that
  the helper (`evorch-cef-helper`) exists and registers the artifact pages
  and the tool, so the tool schema does not change after startup. CEF is
  initialized on the GUI thread (which also shuts it down) when the first HTML
  preview, card snapshot or `inspect_artifact` call needs it.
  - An `inspect_artifact` call that arrives first asks the GUI thread to
    initialize CEF and waits for it up to the page timeout (30 seconds). A
    request that timed out does not start CEF later.
  - If initialization fails, the tool call returns it as an error; the
    registered tool is not removed. The result, success or failure, is kept, so
    later calls do not try again. A preview that cannot start goes to the XDG
    Portal.
  - While the window is hidden or minimized, eframe skips `ui` but still runs
    `logic`. The request is served in the `logic` pass, so tool calls do not
    depend on the window drawing frames.
- Frame rates: off-screen rendering is capped at 30 fps for the preview window
  and 10 fps for agent pages. Card snapshots stay at 60 fps: a lower rate
  exposed a blank-frame capture in the isolation test when it was tried.
- Builds without CEF, and runtimes without the GUI, do not offer the tool.
- Why it is separate from `browser_*`:
  - It reaches different agents.
  - It needs no network permission.
  - It has no URL parameter that would need refusing.

Rendering is headless (`--ozone-platform=headless`) with the GPU disabled. The
Chromium sandbox relies on unprivileged user namespaces; the SUID
`chrome-sandbox` is not shipped. `EVORCH_CEF_NO_SANDBOX=1` disables the sandbox
on hosts without user namespaces, `EVORCH_CEF_HELPER` overrides the helper path
(default: `evorch-cef-helper` beside the executable) and `EVORCH_CEF_LOG=<file>`
writes a verbose CEF log.

### Building

- Nix: on x86_64-linux, `nix build` (`packages.default`, also `.#evorch-gui`)
  is the CLI and tools from `.#evorch` with `bin/evorch-gui` starting
  `evorch-gui-cef`. Other systems get `.#evorch`, which has no CEF, and
  `nix build .#evorch` still builds that version anywhere.
  `nix build .#evorch-gui-cef` builds the CEF GUI alone. It fetches the CEF
  build cef-rs pins with a fixed hash, strips `libcef.so`, keeps the `en-US`
  and `ja` locales, and installs `evorch-gui-cef` with the helper and CEF files
  in one `libexec` directory.
- Development: `nix develop .#cef` sets `CEF_PATH` to that distribution and puts
  `target/debug` (where cef-dll-sys copies `libcef.so`) on `LD_LIBRARY_PATH`.
  Outside Nix, cef-dll-sys downloads CEF into `CEF_PATH` on the first build.

```sh
nix develop .#cef
cargo build -p gui -p browser-cef --features gui/cef,browser-cef/cef \
  --bin evorch-gui --bin evorch-cef-helper
cargo run -p gui --features cef --bin evorch-gui
```

### Tests

The pure parts (frame upload, input translation including IME, popup placement,
the URL allowlist) run in the normal workspace suite. The end-to-end tests need
CEF and are ignored by default; the `cef-e2e` CI job runs them.

```sh
nix develop .#cef -c cargo nextest run -p browser-cef --features cef --run-ignored all
```

`offline_isolation` serves a page that probes fetch, XHR, WebSocket, beacon,
WebRTC, file URLs, `window.open`, forms and navigation against a loopback
listener. It then asserts that no connection arrived, the CSP fired and WebRTC
is gone (checked through the in-process DevTools session). It also checks that
a killed renderer reloads the artifact and that a snapshot captures the page
after its scripts ran.

`interaction` drives the preview pane (`CefView`) through egui_kittest, with a
test clipboard in place of egui's. It opens a `<select>` and checks that the
popup is drawn over the pane and that clicking an option selects it. It answers
`confirm`, `prompt` and `alert` in the modal and checks what the page received.
It copies with `execCommand`, `navigator.clipboard.writeText` and Ctrl+C after
`select()`, checks that each copy reached the clipboard, and checks that
`readText` is denied. It also checks that an agent page settles its dialogs and
reports them.

The ignored tests in `gui`'s `html_preview::tests` need the helper path. They
are:

- `card_snapshot_requests_render_through_cef`: drives a card's snapshot
  request through CEF.
- `logic_only_initializes_then_opens_again_without_any_gui_pass`: registers
  agent pages without starting CEF, initializes it from a `logic` pass alone
  (no `ui` pass), and opens a second page without another GUI pass.
- `agent_operates_the_artifact_it_presented_through_cef`: runs a scripted
  conversation root Worker. It captures and presents a counter mock, opens it
  with `inspect_artifact`, clicks, types, chooses a `<select>` option and
  hovers by ref, and takes a screenshot.
  Throughout, no connection may reach a loopback listener.

```sh
nix develop .#cef
cargo build -p browser-cef --features cef --bin evorch-cef-helper
EVORCH_CEF_HELPER=$PWD/target/debug/evorch-cef-helper \
  cargo nextest run -p gui --features cef --lib --run-ignored only \
  -E 'test(/^html_preview::tests::/)'
```
