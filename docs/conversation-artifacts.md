# Conversation artifacts

Agents can show images and HTML mocks to the user as cards in a conversation,
for example to compare UI layouts. Capturing and presenting are separate
permissions ([ADR 0029](../intents/evorch/decisions/0029-conversation-artifacts.md)).

## Tools

`render_artifact` stores the current content of a workspace file and returns an
`artifact_id`. It does not show anything to the user. It is available to:

- a Worker delegated with the `visual` category;
- the conversation's own root Worker (`conversation` category).

`present` shows up to eight captured artifacts as one card in the conversation.
Only the conversation owner may call it: the root Worker or root Orchestrator of
a user-facing conversation. Delegated agents return their `artifact_id` values in
their result, and the Orchestrator presents them.

Both conditions come from the runtime (category, root, purpose), not from model
input. Each tool checks them again when it runs.

## Capture rules

- The file must be a regular file inside the run's workspace after symlinks are
  resolved. Relative paths resolve from the workspace root.
- Supported formats: `.png`, `.jpg`/`.jpeg`, `.gif`, `.webp` up to 8 MiB, and
  `.html`/`.htm` up to 2 MiB. The content must match the extension.
- HTML is rejected when the secret guard detects a credential in it.
- HTML must be self-contained. Only the single file is captured, so linked
  stylesheets, scripts or images will be missing when the user opens it.
- Each capture is an immutable copy. Editing or deleting the working file never
  changes what was presented, so capture again after each revision.

## Presentation rules

- Every listed artifact must belong to the presenting conversation: it was
  captured in the same delegation tree, or by an earlier root of the same thread
  (for example before an escalation). If any id is invalid, nothing is shown.
- The tool result contains `[artifact <id>: <title>]` references, which remain in
  the model's history after compaction.

## Storage and display

`evorch-gui` stores captures in `artifacts/` next to its database:
`blobs/<sha256>.<ext>` holds content and `meta/<artifact_id>.json` holds the
owner, thread and source path. There is no size quota or cleanup yet.

The conversation card shows the title and caption, a scaled preview for images,
and an **Open** button. Open passes the stored file to the desktop's default
application through the XDG Desktop Portal (`OpenFile`), so HTML mocks open in
the browser rather than the in-app file viewer. A card whose stored file is
missing shows "Artifact unavailable".

Presentations are `ToolEvent::ArtifactsPresented` events. Restored
conversations replay them and show the same cards; replay grants no tool
authority.

## Not yet supported

HTML snapshots in the card, multiple viewports, opening in the Browser pane,
returning captures to the model for self-review, and attaching artifacts to
`ask_user` choices are later phases of ADR 0029.
