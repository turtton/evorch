## Goal

Open any sidebar thread as its own dock conversation pane (Obsidian-style ctrl+click), bound to that thread id, so several thread conversations can be viewed in parallel. The main conversation pane keeps following the active thread; extra panes never retarget.

## Why This Slice Exists Now

The transcript model already stores one transcript per thread (`TranscriptKey::Thread`), and thread badges made subagent lineage visible. What remains single-view is the pane: `WorkbenchState::transcript()` renders either the active thread or one focused agent run. Comparing two threads currently means switching the active thread back and forth and losing the previous view.

## Current Observed State

- Only one conversation is visible at a time (`ConversationFocus` in `crates/gui/src/app/state.rs`).
- Sidebar click switches `active_thread`, which retargets the single conversation pane.
- Thread-bound streaming already routes per thread in the model layer; nothing renders a non-active thread.

## Accepted Baseline You May Assume

- Per-thread routing and continuation root-rebinding in `TranscriptRegistry` are stable and tested; do not modify them.
- `PanelKind` + `target` serde persistence with validators in `workspace-ui`; new variants are allowed without migration (AGENTS.md pre-v1 compatibility policy).
- egui_dock auto-open/park rules from the subagent region: no focus-stealing APIs (`push_to_first_leaf` / `push_to_focused_leaf` / `set_active_tab`); preserve dock focus like `subagent_dock.rs` does.

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/src/app/state.rs, crates/gui/src/app/tab_viewer.rs, crates/gui/src/app/tab_viewer/conversation.rs, crates/gui/src/app/actions.rs, crates/gui/src/app/frame.rs, crates/workspace-ui/src/panels.rs, crates/workspace-ui/src/validate.rs, crates/gui/tests/`

Target part: thread-bound conversation panes — per-pane thread binding on the view layer.

## In Scope

- New `PanelKind` for thread conversation with required `target = thread_id` (registration + validation).
- Sidebar ctrl+click (or explicit equivalent) → idempotent open of a thread-bound pane; focus existing tab if already open.
- Conversation renderer bound to an explicit thread id per pane; main pane behavior unchanged.
- Extra thread panes are read-only (composer stays on the active-thread main conversation).
- Live streaming into thread-bound panes; archived/closed thread panes render persisted transcript without panics or empty dock nodes.
- Layout save/load roundtrip preserving thread-bound panes.

## Out Of Scope

- Per-pane composers or cross-thread input.
- Transcript routing / subagent region / docking policy changes.
- Multi-window support.

## Standalone Child Issue Contract

Deliver a dock pane kind that renders a single thread's conversation bound by thread id: openable via ctrl+click from the sidebar (idempotent), live-updating from that thread's routed transcript, read-only in this slice, persisted across layout save/load, with no focus stealing and no retargeting when the active thread changes. All existing conversation, dock, and layout tests must stay green.

## Acceptance Criteria

- Ctrl+click opens a thread-bound pane once; repeated open focuses the existing tab.
- Active-thread switch does not alter thread-bound pane content; main conversation still follows the active thread.
- Bound pane live-renders the bound thread's root-run stream.
- Closed/archived thread panes render persisted transcript; dock export has no `EmptyNode`.
- Layout roundtrip preserves kind + target + tab state.

## Verification

- New headless tests under `crates/gui/tests/`; `cargo test -p gui` and `cargo test -p workspace-ui` green; dock/layout canaries (`dock_roundtrip`, `layout_v02`, `terminal_default_layout`, `right_panes_headless`, `approvals_layout_headless`) green; `cargo build --workspace`; `cargo fmt --all --check`; `git diff --check`.

## Related Links

- `intents/evorch/features/gui-workbench/overview.md`
- AGENTS.md "Compatibility policy (pre-v1)"

## Knowledge Maintenance

- Intent placement: gui-workbench overview (no new node)
- ADR candidate: none
- Diagram candidate: none
- Docs update: `intents/evorch/features/gui-workbench/overview.md` (per-pane thread binding + affordance)
- Closeout writeback expected: yes — read-only sufficiency and concurrent-render routing findings

## Guide Reachability (G645)

Guide surface: workflow task implementation-loop (role: implementation) → thread-bound conversation panes (sidebar ctrl+click open). Declared in packet.yaml; not leaving blank.

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
