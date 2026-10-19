# Embedded Browser (opt-in)

Build/run with `cargo run -p gui --features browser --bin evorch-gui`.
The Chromium session (launch, CDP, diagnostics) lives in `crates/browser` so
runtime and tools can use it too. The GUI pane is behind the opt-in `browser`
feature; default GUI builds do not link Chromiumoxide.
Install Chromium separately. No browser is downloaded or opened automatically.

Open **Browser**, then choose **Start browser**. Headless is the default.
**Headful (starts minimized)** applies only to the next session: Stop, change
the toggle, then Start. Headful creates a background, minimized window; evorch
never calls an activation/bring-to-front API. The desktop compositor owns
final focus policy. Closing the pane stops the session.

Enter an HTTP(S) URL and Navigate. A CSS selector plus Click element executes
an element click. This is a screencast inspection pane, not full keyboard or
pointer forwarding. Each session uses a unique temporary Chromium profile.
The Chrome sandbox and TLS certificate checks are not disabled.

`EventKind::Diagnostic` records `browser.action`, `browser.screenshot` and
`browser.dom_diff`; screenshot detail contains JPEG base64, and diff detail
contains a Unicode character prefix plus removed/inserted text. Changes larger
than 16,384 characters are marked truncated. Before/after refer to command
completion, not network-idle or arbitrary application animation completion.
Diagnostics flow through the existing event bus/storage bridge. Screenshots
and DOM can contain sensitive page data: use only sessions suitable for local
diagnostic retention. Do not navigate to confidential pages unintentionally.

Memory is bounded by an eight-command queue and one latest decoded frame;
screencasts are limited to 1280x720 and decoded with image allocation limits.
No screenshot is logged on every video frame, only around explicit actions.

## Agent browser tools

When a Chromium or Chrome executable is installed (or named by `CHROME`), the
runtime registers `browser_navigate`, `browser_navigate_back`,
`browser_navigate_forward`, `browser_snapshot`, `browser_click`,
`browser_hover`, `browser_type`, `browser_select_option`, `browser_upload_file`,
`browser_press_key`, `browser_scroll`, `browser_handle_dialog`,
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
max_pages = 8                     # more closes the least recently used page
coordinate_actions = false        # true also registers browser_click_at
```

`browser_click_at` clicks a point given in CSS pixels from the top-left of the
visible viewport, as `browser_screenshot` shows it. It is for targets the
outline cannot name, such as a canvas; refs stay the default.

- One headless Chromium is launched on first use, with a temporary profile.
  Each run gets its own page; at most `max_pages` stay open and a run's page
  closes when the run ends or on `browser_close`.
- Pages are read as an accessibility outline. Elements carry `[ref=eN]`, and
  the action tools accept only refs from the latest outline. Actions return
  the updated outline; `browser_screenshot` returns a PNG image result.
- `browser_navigate` URLs go through the same per-call review as `web_fetch`.
  Back and forward revisit history entries, which load under the same
  request policy; the back/forward cache is disabled, so every entry loads
  again.
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
- Popups are refused (`--block-new-web-contents`), QUIC is disabled and WebRTC
  is limited to proxied UDP.

### Watching the agent browser

In GUI builds with the `browser` feature, **Agent browser** opens a read-only
window onto the runtime's agent pages. Every executor of the process, including
the ones runs build for their own sandbox, shares one agent browser, so the
window lists every run that has an open page. Only the selected run's page is
screencast, and only while the window is open; closing the window stops it.
The latest actions (every browser tool call, with failures), refused requests
and closed dialogs are laid over the frame. Typed text is
shown up to 40 characters. The window never sends input to the page.

## Low-memory verification

Use `CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

- `cargo test -p gui --features browser --test browser`
- `cargo test -p browser`
- With installed Chromium: `cargo test -p browser --lib tests::chromium_screencast_and_action_evidence -- --ignored`
- With installed Chromium: `cargo test -p browser --lib tests::chromium_receives_the_session_switches -- --ignored`
- With installed Chromium: `cargo test -p browser --test agent_chromium -- --ignored`

The Chromium integration test uses only an ephemeral localhost HTTP fixture.
It is ignored by the normal test run to avoid launching Chromium implicitly.
See [gui-verification.md](gui-verification.md) for the full GUI verification
layer matrix, where this test is layer L6.

## In-app HTML preview (CEF, opt-in)

The `cef` feature of `gui` opens presented HTML artifacts (`.html`/`.htm`) in
an in-app window rendered off-screen by CEF (`crates/browser-cef`, ADR 0034 B).
Other artifacts, and every artifact when CEF cannot start, still go to the XDG
Portal. Builds without the feature are unchanged and never fetch CEF.

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
DevTools. The actions `snapshot`, `click`, `type`, `press_key`, `scroll` and
`screenshot` work as their `browser_*` counterparts. The tool takes no URL and
the page cannot navigate, so nothing leaves CEF. Refused navigations and
popups are listed in the next outline. Nobody sees the page's JS dialogs:
`alert` is closed, `confirm` and `prompt` are dismissed (`false` and `null`),
`beforeunload` leaves, and the next outline lists each under "Dialogs:".

- Who can use it: visual Workers, and the conversation's root Worker or root
  Orchestrator, the same agents that capture or present artifacts. Ownership
  is checked against the artifact store as for `present`.
- Pages: each run has one page, and at most four are open at once. A run's
  page closes when the run ends or on `close`.
- Startup: the GUI starts CEF when its window is created, so tool calls do not
  depend on the window drawing frames.
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

- Nix: `nix build .#evorch-gui-cef` (x86_64-linux) fetches the CEF build cef-rs
  pins with a fixed hash, strips `libcef.so`, keeps the `en-US` and `ja`
  locales, and installs `evorch-gui-cef` with the helper and CEF files in one
  `libexec` directory. The default package does not include CEF.
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
- `agent_operates_the_artifact_it_presented_through_cef`: runs a scripted
  conversation root Worker. It captures and presents a counter mock, opens it
  with `inspect_artifact`, clicks and types by ref, and takes a screenshot.
  Throughout, no connection may reach a loopback listener.

```sh
nix develop .#cef
cargo build -p browser-cef --features cef --bin evorch-cef-helper
EVORCH_CEF_HELPER=$PWD/target/debug/evorch-cef-helper \
  cargo nextest run -p gui --features cef --lib --run-ignored only \
  -E 'test(/^html_preview::tests::/)'
```
