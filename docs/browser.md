# Embedded Browser (opt-in)

Build/run with `cargo run -p gui --features browser --bin evorch-gui`.
The default feature set is empty; ordinary builds do not compile Chromiumoxide.
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

## Low-memory verification

Use `CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

- `cargo test -p gui --features browser --test browser`
- `cargo test -p gui --features browser --lib browser::diagnostics`
- With installed Chromium: `cargo test -p gui --features browser --lib browser::tests::chromium_screencast_and_action_evidence -- --ignored`

The Chromium integration test uses only an ephemeral localhost HTTP fixture.
It is ignored by the normal test run to avoid launching Chromium implicitly.
See [gui-verification.md](gui-verification.md) for the full GUI verification
layer matrix, where this test is layer L6.
