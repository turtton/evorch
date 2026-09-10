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

## 8. Verification and debt
Use egui headless interaction tests for controls and native screenshot evidence
where a graphics backend is available. Browser Lighthouse does not apply.
Browser frame/texture tests use an injected FrameSource; CDP tests use local HTTP.
The screencast is a raster inspection view, not an accessibility tree. Native
compositor focus behavior requires platform QA.
