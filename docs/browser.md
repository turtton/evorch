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
runtime registers `browser_navigate`, `browser_snapshot`, `browser_click`,
`browser_type`, `browser_press_key`, `browser_scroll`, `browser_screenshot` and
`browser_close` (ADR 0034). WebResearcher and the conversation root Worker may
use them; `sandbox.web_tools_enabled` is their global switch, as for the other
Web tools.

- One headless Chromium is launched on first use, with a temporary profile.
  Each run gets its own page; at most eight stay open and a run's page closes
  when the run ends or on `browser_close`.
- Pages are read as an accessibility outline. Elements carry `[ref=eN]`, and
  the action tools accept only refs from the latest outline. Actions return
  the updated outline; `browser_screenshot` returns a PNG image result.
- `browser_navigate` URLs go through the same per-call review as `web_fetch`.
  Every request of the page, including redirects, subresources and frames, is
  paused with the CDP Fetch domain and checked like `web_fetch` (HTTPS only,
  blocked link-local and CGNAT addresses). Refused requests are listed in the
  next result.
- Known gaps: Chromium resolves hosts itself, so the checked addresses are not
  pinned; WebSocket connections and service-worker fetches are not paused.
  Popups are refused (`--block-new-web-contents`) and WebRTC is limited to
  proxied UDP.

## Low-memory verification

Use `CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

- `cargo test -p gui --features browser --test browser`
- `cargo test -p browser`
- With installed Chromium: `cargo test -p browser --lib tests::chromium_screencast_and_action_evidence -- --ignored`
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
each renderer. JS dialogs, file choosers, popups and the context menu are
suppressed; `<select>` dropdowns and the clipboard are not wired yet. A crashed
renderer is reloaded up to three times.

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

The pure parts (frame upload, input translation including IME, the URL
allowlist) run in the normal workspace suite. The end-to-end test needs CEF
and is ignored by default; the `cef-e2e` CI job runs it.

```sh
nix develop .#cef -c cargo nextest run -p browser-cef --features cef --run-ignored all
```

It serves a page that probes fetch, XHR, WebSocket, beacon, WebRTC, file URLs,
`window.open`, forms and navigation against a loopback listener, then asserts
that no connection arrived, the CSP fired, WebRTC is gone (checked through the
in-process DevTools session) and a killed renderer reloads the artifact.
