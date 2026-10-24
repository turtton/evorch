# Conversation artifacts

Agents can show images and HTML mocks to the user as cards in a conversation,
for example to compare UI layouts. Capturing and presenting are separate
permissions ([ADR 0029](../intents/evorch/decisions/0029-conversation-artifacts.md)).

## Tools

`render_artifact` stores the current content of a workspace or `$TMPDIR` file and
returns an `artifact_id`. It does not show anything to the user. It is available to:

- a Worker delegated with the `visual` category;
- the conversation's own root Worker (`conversation` category).

`present` shows up to eight captured artifacts as one card in the conversation.
Only the conversation owner may call it: the root Worker or root Orchestrator of
a user-facing conversation. Delegated agents return their `artifact_id` values in
their result, and the Orchestrator presents them.

`inspect_artifact` opens an HTML artifact of the caller's conversation in an
offline CEF page. The agent reads the page as an accessibility outline, clicks
or types by ref, and takes screenshots. Only `evorch-gui` built with the `cef`
feature (the Nix default package on x86_64-linux) offers it
([browser.md](browser.md)). The agents that may capture or
present artifacts may use it.

These conditions come from the runtime (category, root, purpose), not from model
input. Each tool checks them again when it runs.

## Capture rules

- The file must be a regular file inside the run's workspace or its thread's
  temporary directory (`$TMPDIR`, [ADR 0033](../intents/evorch/decisions/0033-file-tool-path-boundary.md))
  after symlinks are resolved. Relative paths resolve from the workspace root.
  Another thread's `$TMPDIR` and allowed directories
  ([ADR 0032](../intents/evorch/decisions/0032-project-allowed-directories.md)) are not
  sources, even when file tools can read them. Reference material and drafts
  can stay in `$TMPDIR` so they do not touch the repository. Reads use retained
  root directory descriptors, refuse symlinks in every resolved path component,
  and check the opened descriptor is a regular file; concurrent symlink or FIFO
  replacements cannot redirect or block a capture.
- Supported formats: `.png`, `.jpg`/`.jpeg`, `.gif`, `.webp` up to 8 MiB, and
  `.html`/`.htm` up to 2 MiB as written. The content must match the extension.
- HTML is rejected when the secret guard detects a credential in it. The guard
  checks the HTML as written, before images are inlined.
- Each capture is an immutable copy. Editing or deleting the working file never
  changes what was presented, so capture again after each revision.

### Images referenced by HTML

The stored HTML must not need other files, because the preview loads nothing
else ([browser.md](browser.md)). Capturing HTML therefore inlines the images it
references by path ([ADR 0038](../intents/evorch/decisions/0038-image-generation.md)):

- `<img src>` values and CSS `url()` in `<style>` elements and `style`
  attributes become `data:` URIs. Paths resolve like a browser opening the real
  HTML file (after symlinks): relative to its directory, `/` from the
  filesystem root, with `?query` and `#fragment` ignored and percent escapes
  decoded. Attribute values decode the complete WHATWG character-reference
  table (including legacy names), and CSS uses tokenization for escaped `url`
  names, strings and comments. CSS nested more than 128 blocks/functions is
  rejected rather than leaving deep references unresolved.
- References with a scheme (`http:`, `https:`, `data:`, `file:`, `blob:`, …), a
  host (`//host/…`), only a fragment (`#id`) or no value are kept as written.
  Comments, strings and raw text such as `<script>`, `<textarea>` and
  `<noscript>` are not read.
- Each referenced file must be a `.png`, `.jpg`/`.jpeg`, `.gif` or `.webp` of up
  to 8 MiB whose content matches its extension, inside the workspace or the
  same `$TMPDIR`. If any reference is missing, outside, another format (for
  example a font) or too large, nothing is stored and the error lists the
  failing references with the reason (the first eight, then a count).
- The stored HTML, with its images inlined, may be up to 8 MiB.
- `srcset`, `<link>` stylesheets, `<script src>`, CSS `image-set()` and fonts are
  not inlined; put CSS and scripts in the file itself.
- The metadata lists each inlined file once (`inlined_assets`: source path,
  SHA-256 and size). HTML without such references and images are stored
  byte for byte as before.

## Presentation rules

- Every listed artifact must belong to the presenting conversation: it was
  captured in the same delegation tree, or by an earlier root of the same thread
  (for example before an escalation). If any id is invalid, nothing is shown.
- The tool result contains `[artifact <id>: <title>]` references, which remain in
  the model's history after compaction.

## Storage and display

`evorch-gui` stores captures in `artifacts/` next to its database:
`blobs/<sha256>.<ext>` holds content and `meta/<artifact_id>.json` holds the
owner, thread, source path and inlined images. There is no size quota or
cleanup yet.

The conversation card shows the title and caption and a scaled preview for
images.

- Images: clicking the preview opens an in-app viewer over the workbench. The
  wheel or a pinch zooms around the pointer, dragging pans, and double-click
  switches between fitting the window and actual size (one image pixel per
  screen pixel). The toolbar and the keys `+`, `-`, `0` (fit) and `1` (actual
  size) do the same, and Esc closes it. **Open in external app** in the
  preview's or the viewer's context menu passes the file to the desktop's
  default application through the XDG Desktop Portal (`OpenFile`).
- HTML mocks: an **Open** button opens them in an in-app CEF preview, and their
  cards show a snapshot, in builds with the `cef` feature: the Nix default
  package on x86_64-linux ([browser.md](browser.md)). Other builds pass HTML to
  the Portal, so it opens in the browser rather than the in-app file viewer,
  and the card shows no preview.

A card whose stored file is missing shows "Artifact unavailable".

Presentations are `ToolEvent::ArtifactsPresented` events. Restored
conversations replay them and show the same cards; replay grants no tool
authority.

## Not yet supported

Multiple viewports, self-review of HTML in builds without CEF, and attaching
artifacts to `ask_user` choices are later phases of ADR 0029. With the `cef`
feature, cards already show HTML snapshots and open in-app
([browser.md](browser.md)).
