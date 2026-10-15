# Existing desktop workbench design

## 1. Direction
Preserve the operational egui workbench; no redesign or new visual dependencies.

## 2. Color
Use `theme/tokens.rs`: CANVAS, SURFACE, TEXT, TEXT_MUTED, ERROR_FG.
Status is expressed in text as well as color.

## 3. Typography
Use the installed egui theme: body 14, small/mono 12. IDs use monospace.

## 4. Spacing
Use SP_1 through SP_4 (4/8/12/16). Panels own their scrolling.

## 5. Primitives
Reuse egui TextEdit, ComboBox, Button, ScrollArea, Grid and collapsing headers.
Keep native keyboard focus and selected/hover/disabled states.
Routing uses named collapsing headers: registered routes start closed, new routes
start open, and expansion persists through saves within the modal session.
Usage labels remain visible when collapsed. Candidate models span declared profiles;
subscription profiles precede alphabetically ordered API profiles.
Thread archive uses a Painter-drawn box at Small text height, a 1.2pt stroke,
a wider lid with a centered handle slit, and native interaction text color.
Its native small Button reserves Small text height + SP_2 on each axis;
pinned threads use egui's disabled painting and cannot activate Archive.
Workspace contention adds a Painter-drawn hourglass in WARNING_FG alongside the
thread's existing status and question indicators, including waits from subagents.
Its tooltip lists elapsed wait, waiting tool, authoritative current holder and
workspace; shell commands use bounded, redacted previews. Unknown holders remain
explicitly unknown. Only visible tooltips request elapsed-time repainting.
Sandbox settings reuse the settings modal and native labeled Checkbox with
unchecked/checked/focused states, Save/Cancel, inline errors and explicit scope text.
Conversation message/reasoning cards show a small TEXT_MUTED `[role]` label
above the body, resolved from the entry's run ID and current task rows.
Unknown runs and legacy entries omit the label; no role is inferred from focus.
Reasoning uses a lowercase `thinking` collapsing header in TEXT_MUTED, with a
wrapped, muted italic body. Live reasoning opens; the following answer or run
completion closes it once. History starts closed. Native keyboard and pointer
toggles remain available, and manual expansion persists across subsequent frames.
Expanded tool-call titles stay at the transcript viewport's top while their card
is being read, then leave with the card's bottom edge. The original title button
and shell-log action retain their identities; collapsed/running calls do not
stick. Collapsing a pinned call reveals its normal title position without an
animation. Output continues to use the transcript's single vertical scroll area.
One tool call is one card: approval states update the original card, and sandbox
escape and network access reviews show as a small line under its title, visible
while collapsed; denied or declined reviews use WARNING_FG and a shield-warning icon.
Reviews without a card (legacy history) render standalone. Coordination tools name
their agents (`Delegate worker-w1 (worker)`, `Message → reviewer-r1 · preview`).
Agent messages read `→ name (role): text`; unknown runs show a shortened ID.
Compaction shows one `Context compacted` line; range, threshold, window,
checkpoint and summary sit in a closed `details` section with a custom header.
Memory uses labeled search and status controls, result count, expandable evidence,
explicit empty/error states and refresh. Tasks retains its existing grid.
Browser reuses pane_root, labeled inputs, wrapping toolbars and aspect-preserving
textures. States are stopped, starting, streaming and error. Stop remains available.

## 6. Layout
Dock panels remain movable. Wrap long lesson text and scroll wide task grids.
Memory and task dependencies are supplementary panels, not modal workflows.
Browser is an opt-in egui window. Start is explicit; headful selection applies to
the next session and creates a minimized background window. Closing stops it.
Never request external focus or activate a browser target from the UI.

## 7. Accessibility
Label every input, retain keyboard focus, and never use color as the only status.
No motion is introduced. Long IDs must remain inspectable without clipping.
Transcript prose avoids inline run, call and question IDs; `copy_id_button` keeps
them available, with the full ID on hover.
The ownership header abbreviates owner IDs to five characters plus `…`, with the
full ID, generation and state on hover. Access retains the last confirmed state
(or `checking ownership` initially) while probes retry. SQLite contention gets a
one-second grace before an access-label `⚠` in WARNING_FG; other failures warn
immediately. Retry details stay in the tooltip, never a raw header error or an
agent-stopped status. Action errors stay beside ownership buttons or in the close
dialog. Cached display state never authorizes writes; live ownership checks do.

## 8. Verification and debt
Use egui headless interaction tests for controls and native screenshot evidence
where a graphics backend is available. Browser Lighthouse does not apply.
Browser frame/texture tests use an injected FrameSource; CDP tests use local HTTP.
The screencast is a raster inspection view, not an accessibility tree. Native
compositor focus behavior requires platform QA.
