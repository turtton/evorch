# Vendored crates

| Crate | Upstream | Local change |
|---|---|---|
| `egui-winit` 0.36.2 | <https://github.com/emilk/egui> | Emit an empty keyboard-paste signal when no text is available, so a focused composer can read an OS clipboard image; nonempty text and RequestPaste are unchanged |
| `egui_commonmark` 0.25.0 | <https://github.com/lampsitter/egui_commonmark> | `CommonMarkViewer::link_menu_fn` builder |
| `egui_commonmark_backend` 0.25.0 | same | `RenderLinkMenuFn`, `CommonMarkOptions::link_menu_fn`, attach it to hyperlinks in `Link::end` |

The first commit that added each crate is the unmodified crates.io release, so
`git log -p vendor/` shows the local patch on its own. Remove the crate and its
`[patch.crates-io]` entry in the root `Cargo.toml` once upstream ships an
equivalent hook.

The shared `depsOnlyArgs.extraDummyScript` in `flake.nix` preserves this vendor
tree in Crane’s dependency-only source. Registry crates such as `eframe` need
the patched crates’ real public APIs; empty workspace stubs cannot satisfy them.
