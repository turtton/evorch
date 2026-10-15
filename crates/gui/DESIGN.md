# Existing desktop workbench design

## 1. Direction
Preserve the operational egui workbench; no redesign or new visual dependencies.

## 2. Color
Use `theme/tokens.rs`: CANVAS, SURFACE, TEXT, TEXT_MUTED, ERROR_FG.
Status is expressed in text as well as color.
Run status text and color come only from `phase_label`/`agent_phase_label` and
`phase_color`/`agent_phase_color`: Pending, Running, Waiting for input, Done,
Failed, Stopped (resumable); owner-cancelled runs read Cancelled in TEXT_MUTED.
Questions and approvals awaiting the user use `attention_color` (WARNING_FG);
error text uses ERROR_FG.

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
Thread titles open a right-click menu with Copy thread ID / Copy thread title.
Pending tool approvals, including a subagent's, mark the owning thread with the
question's WARNING_FG dot and "Approval needed" text; beside a question dot they
use a SHIELD_WARNING glyph instead so both stay distinguishable.
Quota bars fill SUCCESS above 50% remaining, WARNING_FG from 20–50% and ERROR_FG
below 20%; the percentage stays in the summary, tooltip and accessible value.
Reset and expiry times read relative ("resets in 1d 5h 30m"), with the absolute
UTC time in the same detail line or tooltip; unknown stays "resets unknown".
Saved/refresh/candidate times read as "5m 12s ago" with absolute UTC on hover.
File previews have no reload button: a visible preview stats its file once per
second and, on an mtime/size change or (dis)appearance, loads the new version in
the background, swapping it in only when ready so content never flashes.
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
Agent rows read `model · provider profile` (provider kind as fallback, kind on
hover); tokens use `format::compact_count`, TTFT `format::latency`, Δ
`format::elapsed`; exact counts stay in the tooltip.
One tool call is one card: approval states update the original card, and sandbox
escape and network access reviews show as a small line under its title, visible
while collapsed; denied or declined reviews use WARNING_FG and a shield-warning icon.
Reviews without a card (legacy history) render standalone. Coordination tools name
their agents (`Delegate worker-w1 (worker)`, `Message → reviewer-r1 · preview`).
Agent messages read `→ name (role): text`; unknown runs show a shortened ID.
Compaction shows one `Context compacted` line; range, threshold, window,
checkpoint and summary sit in a closed `details` section with a custom header.
The composer panel is sized from measured parts every frame. The input grows
with wrapped rows up to COMPOSER_MAX_HEIGHT before expanded goal/procedure
details, which shrink to a scroll viewport of at least ROW_DENSE; one
transcript row stays visible. Only in-progress procedures use TEXT and a
half-filled circle; pending (circle) and completed (check) use TEXT_MUTED.
The sandbox button shows a shield and the escalation mode (auto review, ask me,
always deny), explained on hover. Stop halts the current run; a stopped run with
live subagents shows Stop all for the subtree. Both tooltips name the scope.
Self-improvement drafts settings sit in the ⚙ menu, not the composer.
The thread header shows `Total cost (incl. subagents)`; the status strip cost
covers the main agent only and says so on hover. Wall time uses format::duration.
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
Runs read by agent name (else thread title), never by run ID; the ID sits behind
`copy_id_button` (named `Copy run ID`, full value on hover). Notification
summaries name runs the same way. Clicking an agent name drills into its run.
Transcript prose avoids inline run, call and question IDs; `copy_id_button` keeps
them available, with the full ID on hover.
Agent headers title `name / role`; the run ID sits behind a Copy run ID button
that shows the full ID on hover.
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
