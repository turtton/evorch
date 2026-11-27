//! The core client's own session file, `core-layout.json` in the configuration directory:
//! the dock's layout, the run tabs and the selection, so the window opens as it was left.
//! It is this machine's view of the core. Nothing of it is stored in the core, and the
//! embedded workbench's `ui.toml` is read for the theme and the keymap, and written only
//! for the theme preset (`theme.rs`).
//!
//! Each section is decoded on its own, so a selection that no longer decodes keeps a good
//! layout. A layout that does not validate gives the default dock and is replaced by the
//! next save; nothing in the file can make the window panic. A file that cannot be read
//! at all (another version, broken JSON) is replaced only by an explicit save.
//!
//! The dock's tabs are not stored as [`CoreTab`]s: a run or file tab is an id that only
//! means something in one session. A File tab is kept by its absolute path, as the
//! embedded workbench keeps its file viewers. A run tab is kept out of the tree, by its
//! thread and run, and comes back beside Execution when its thread is selected again.
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use egui_dock::{
    DockState, Node, NodeIndex, NodePath, Split, Surface, SurfaceIndex, TabIndex, Tree,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workbench_protocol::ids::{ProjectId, RunId, ThreadId};
use workspace_ui::{SplitDirection, WindowRect};

use super::features::FeatureId;
use super::local::files::FileTabs;
use super::run_tabs::{MAX_RESTORED, RunPane};
use super::shell::{self, CoreTab};
use crate::dock::{child_path, enforce_min_fraction_left_of};

pub const FILE_NAME: &str = "core-layout.json";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 1 << 20;
/// The deepest a leaf may be. `egui_dock` sizes a surface's node vector by the depth of
/// its deepest node, so a deeper subtree is merged into one leaf rather than built.
const MAX_DEPTH: usize = 10;
/// The most tabs a restored dock has; past them only the required tabs are kept.
const MAX_TABS: usize = 256;
/// The most folded threads a restored sidebar keeps.
const MAX_FOLDED_THREADS: usize = 4096;
/// The most threads with an open lane list a restored sidebar keeps.
const MAX_OPENED_LANES: usize = 4096;
/// Below this width or height a floating window shows no more than its tab bar (folded
/// or minimized). Such a rect is neither kept nor restored.
const MIN_WINDOW_EDGE: f32 = 64.0;
/// How near a saved split fraction comes to 0 or 1. `egui_dock` lets the separator of a
/// split smaller than its `extra` reach either edge, a fraction the loader refuses.
const MIN_SAVED_FRACTION: f32 = 0.01;
/// The tabs that cannot be closed, without which a layout is not used.
const REQUIRED: [SavedTab; 3] = [
    SavedTab::Projects,
    SavedTab::Conversation,
    SavedTab::Notifications,
];

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("the file is larger than {MAX_BYTES} bytes")]
    TooLarge,
    #[error("unsupported version {0}")]
    Version(u32),
}

/// Where the session file is. The window gets one only from [`super::run`]; a screen
/// built without one (tests, e2e) reads and writes nothing.
#[derive(Debug, Clone)]
pub struct ClientStateStore {
    path: PathBuf,
    /// Whether [`Self::load`] found a file it could not read, such as one a newer build
    /// wrote. Only [`Self::save`] replaces it; [`Self::autosave`] leaves it as it is.
    unread: Cell<bool>,
}

impl ClientStateStore {
    pub fn in_config_dir(dir: &Path) -> Self {
        Self::at(dir.join(FILE_NAME))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            unread: Cell::new(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved state, or `None` when there is no file or it cannot be read (logged).
    pub fn load(&self) -> Option<SavedState> {
        match self.read() {
            Ok(state) => state,
            Err(error) => {
                self.unread.set(true);
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "core client layout load failed; using the default layout"
                );
                None
            }
        }
    }

    fn read(&self) -> Result<Option<SavedState>, PersistError> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(PersistError::TooLarge);
        }
        SavedState::decode(&bytes).map(Some)
    }

    /// Saves `state` unless the file is one [`Self::load`] could not read, which is kept
    /// for the build that wrote it; `false` when it was kept.
    pub fn autosave(&self, state: &SavedState) -> Result<bool, PersistError> {
        if self.unread.get() {
            return Ok(false);
        }
        self.save(state).map(|()| true)
    }

    /// Writes `state` whole or not at all: to a temporary file beside the target, then
    /// renamed over it.
    pub fn save(&self, state: &SavedState) -> Result<(), PersistError> {
        let dir = self
            .path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut file, &state.encode()?)?;
        file.write_all(b"\n")?;
        file.persist(&self.path).map_err(|error| error.error)?;
        self.unread.set(false);
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavedState {
    /// `None` when the file has none or it did not validate: the default dock.
    pub layout: Option<Layout>,
    /// The most recently opened first, as [`super::run_tabs::RunTabs::panes`] gives them.
    pub run_tabs: Vec<RunPane>,
    pub selection: Option<SavedSelection>,
    /// The threads whose children the sidebar shows folded.
    pub folded_threads: Vec<ThreadId>,
    /// The threads whose lane list the sidebar shows open; the others keep it collapsed.
    pub opened_lanes: Vec<ThreadId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSelection {
    pub project: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedTab {
    Projects,
    Conversation,
    Notifications,
    Plan,
    Diff,
    Terminal,
    Feature { feature: FeatureId },
    File { path: PathBuf },
}

impl SavedTab {
    /// The workspace feature draws the Projects tab and has no tab of its own, and a
    /// relative path would resolve against this process's working directory.
    fn is_valid(&self) -> bool {
        match self {
            Self::Feature { feature } => *feature != FeatureId::Workspace,
            Self::File { path } => path.is_absolute(),
            _ => true,
        }
    }

    /// The tab of `tab`; a run tab is kept apart from the tree.
    fn of(tab: CoreTab, files: &FileTabs) -> Option<Self> {
        Some(match tab {
            CoreTab::Projects => Self::Projects,
            CoreTab::Conversation => Self::Conversation,
            CoreTab::Notifications => Self::Notifications,
            CoreTab::Plan => Self::Plan,
            CoreTab::Diff => Self::Diff,
            CoreTab::Terminal => Self::Terminal,
            CoreTab::Feature(feature) => Self::Feature { feature },
            CoreTab::File(id) => Self::File {
                path: files.get(id)?.path.clone(),
            },
            CoreTab::Run(_) => return None,
        })
    }

    fn to_tab(&self, files: &mut FileTabs) -> CoreTab {
        match self {
            Self::Projects => CoreTab::Projects,
            Self::Conversation => CoreTab::Conversation,
            Self::Notifications => CoreTab::Notifications,
            Self::Plan => CoreTab::Plan,
            Self::Diff => CoreTab::Diff,
            Self::Terminal => CoreTab::Terminal,
            Self::Feature { feature } => CoreTab::Feature(*feature),
            Self::File { path } => CoreTab::File(files.open(path.clone())),
        }
    }
}

/// The dock: its main surface and its floating windows. The file holds it with each tab
/// as raw JSON (`T = Value`), which validation decodes one by one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout<T = SavedTab> {
    pub main: SavedNode<T>,
    #[serde(default)]
    pub windows: Vec<SavedWindow<T>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow<T = SavedTab> {
    /// `None` when the window's rect is not known: it opens at egui's default size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<WindowRect>,
    pub root: SavedNode<T>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedNode<T = SavedTab> {
    Split {
        direction: SplitDirection,
        fraction: f32,
        first: Box<SavedNode<T>>,
        second: Box<SavedNode<T>>,
    },
    Tabs {
        tabs: Vec<T>,
        #[serde(default)]
        active: usize,
        #[serde(default)]
        collapsed: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct SavedFile {
    version: u32,
    #[serde(default)]
    layout: Value,
    #[serde(default)]
    run_tabs: Value,
    #[serde(default)]
    selection: Value,
    #[serde(default)]
    folded_threads: Value,
    #[serde(default)]
    opened_lanes: Value,
}

#[derive(Serialize, Deserialize)]
struct SavedRun {
    thread: ThreadId,
    run: RunId,
    #[serde(default)]
    name: String,
    #[serde(default)]
    child: bool,
}

impl SavedState {
    fn decode(bytes: &[u8]) -> Result<Self, PersistError> {
        let file: SavedFile = serde_json::from_slice(bytes)?;
        if file.version != VERSION {
            return Err(PersistError::Version(file.version));
        }
        let layout = (!file.layout.is_null())
            .then(|| {
                serde_json::from_value::<Layout<Value>>(file.layout)
                    .map_err(|error| error.to_string())
                    .and_then(|layout| layout.validate().map_err(str::to_owned))
                    .inspect_err(|error| {
                        tracing::warn!(%error, "core client layout dropped; using the default layout");
                    })
                    .ok()
            })
            .flatten();
        let run_tabs = serde_json::from_value::<Vec<Value>>(file.run_tabs)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|run| serde_json::from_value::<SavedRun>(run).ok())
            .take(MAX_RESTORED)
            .map(|run| RunPane {
                thread: run.thread,
                run: run.run,
                name: run.name,
                child: run.child,
            })
            .collect();
        let selection = serde_json::from_value(file.selection).ok().flatten();
        let folded_threads = serde_json::from_value::<Vec<Value>>(file.folded_threads)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|thread| serde_json::from_value::<ThreadId>(thread).ok())
            .take(MAX_FOLDED_THREADS)
            .collect();
        let opened_lanes = serde_json::from_value::<Vec<Value>>(file.opened_lanes)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|thread| serde_json::from_value::<ThreadId>(thread).ok())
            .take(MAX_OPENED_LANES)
            .collect();
        Ok(Self {
            layout,
            run_tabs,
            selection,
            folded_threads,
            opened_lanes,
        })
    }

    fn encode(&self) -> Result<SavedFile, serde_json::Error> {
        let run_tabs: Vec<SavedRun> = self
            .run_tabs
            .iter()
            .map(|pane| SavedRun {
                thread: pane.thread.clone(),
                run: pane.run.clone(),
                name: pane.name.clone(),
                child: pane.child,
            })
            .collect();
        Ok(SavedFile {
            version: VERSION,
            layout: serde_json::to_value(&self.layout)?,
            run_tabs: serde_json::to_value(run_tabs)?,
            selection: serde_json::to_value(&self.selection)?,
            folded_threads: serde_json::to_value(&self.folded_threads)?,
            opened_lanes: serde_json::to_value(&self.opened_lanes)?,
        })
    }
}

impl Layout<Value> {
    /// Drops the tabs that do not decode, are invalid or repeat one before them, those
    /// past [`MAX_TABS`] but the required ones, then the leaves left empty; merges a
    /// subtree deeper than [`MAX_DEPTH`] into one leaf, and forgets a window rect too small
    /// to show the window. Refuses a split fraction or a window rect out of range, and a
    /// layout without a required tab.
    fn validate(self) -> Result<Layout, &'static str> {
        let mut seen = Vec::new();
        let mut count = 0;
        let main = validate_node(self.main, 0, &mut seen, &mut count)?
            .ok_or("the main surface has no tab")?;
        let mut windows = Vec::new();
        for window in self.windows {
            let rect = match window.rect {
                Some(rect)
                    if ![rect.x, rect.y, rect.width, rect.height]
                        .iter()
                        .all(|value| value.is_finite())
                        || rect.width <= 0.0
                        || rect.height <= 0.0 =>
                {
                    return Err("a window rect is out of range");
                }
                rect => rect.filter(|rect| rect.width.min(rect.height) >= MIN_WINDOW_EDGE),
            };
            if let Some(root) = validate_node(window.root, 0, &mut seen, &mut count)? {
                windows.push(SavedWindow {
                    rect,
                    root: expand_root(root),
                });
            }
        }
        if REQUIRED.iter().any(|tab| !seen.contains(tab)) {
            return Err("a tab that cannot be closed is missing");
        }
        Ok(Layout {
            main: expand_root(main),
            windows,
        })
    }
}

fn validate_node(
    node: SavedNode<Value>,
    depth: usize,
    seen: &mut Vec<SavedTab>,
    count: &mut usize,
) -> Result<Option<SavedNode>, &'static str> {
    match node {
        SavedNode::Split {
            direction,
            fraction,
            first,
            second,
        } => {
            if depth >= MAX_DEPTH {
                let mut tabs = Vec::new();
                flatten(*first, &mut tabs);
                flatten(*second, &mut tabs);
                let leaf = SavedNode::Tabs {
                    tabs,
                    active: 0,
                    collapsed: false,
                };
                return validate_node(leaf, depth, seen, count);
            }
            if !(fraction.is_finite() && fraction > 0.0 && fraction < 1.0) {
                return Err("a split fraction is out of range");
            }
            let first = validate_node(*first, depth + 1, seen, count)?;
            let second = validate_node(*second, depth + 1, seen, count)?;
            Ok(match (first, second) {
                (Some(first), Some(mut second)) => {
                    // A split is never folded whole: of two folded leaves, the second opens.
                    if let (
                        SavedNode::Tabs {
                            collapsed: true, ..
                        },
                        SavedNode::Tabs { collapsed, .. },
                    ) = (&first, &mut second)
                    {
                        *collapsed = false;
                    }
                    Some(SavedNode::Split {
                        direction,
                        fraction,
                        first: Box::new(first),
                        second: Box::new(second),
                    })
                }
                (one, None) | (None, one) => one,
            })
        }
        SavedNode::Tabs {
            tabs,
            active,
            collapsed,
        } => {
            let mut kept = Vec::new();
            let mut front = 0;
            for (index, value) in tabs.into_iter().enumerate() {
                let Ok(tab) = serde_json::from_value::<SavedTab>(value) else {
                    continue;
                };
                if (*count >= MAX_TABS && !REQUIRED.contains(&tab))
                    || !tab.is_valid()
                    || seen.contains(&tab)
                {
                    continue;
                }
                if index == active {
                    front = kept.len();
                }
                *count += 1;
                seen.push(tab.clone());
                kept.push(tab);
            }
            Ok((!kept.is_empty()).then_some(SavedNode::Tabs {
                tabs: kept,
                active: front,
                collapsed,
            }))
        }
    }
}

/// The tabs of `node`'s leaves, in order.
fn flatten(node: SavedNode<Value>, tabs: &mut Vec<Value>) {
    match node {
        SavedNode::Split { first, second, .. } => {
            flatten(*first, tabs);
            flatten(*second, tabs);
        }
        SavedNode::Tabs { tabs: leaf, .. } => tabs.extend(leaf),
    }
}

fn expand_root(mut root: SavedNode) -> SavedNode {
    if let SavedNode::Tabs { collapsed, .. } = &mut root {
        *collapsed = false;
    }
    root
}

/// The dock `layout` describes (the default one without a layout), with its file tabs
/// registered in `files`. The projects keep at least the sidebar's share of the width.
pub fn restore_dock(layout: Option<&Layout>, files: &mut FileTabs) -> DockState<CoreTab> {
    let Some(layout) = layout else {
        return shell::default_dock();
    };
    let main = to_tabs(&layout.main, files);
    let mut dock = DockState::new(leftmost(&main));
    populate(&mut dock[SurfaceIndex::main()], NodeIndex::root(), &main);
    for window in &layout.windows {
        let root = to_tabs(&window.root, files);
        let surface = dock.add_window(leftmost(&root));
        if let (Some(rect), Some(state)) = (window.rect, dock.get_window_state_mut(surface)) {
            state
                .set_position(egui::pos2(rect.x, rect.y))
                .set_size(egui::vec2(rect.width, rect.height));
        }
        populate(&mut dock[surface], NodeIndex::root(), &root);
    }
    enforce_min_fraction_left_of(&mut dock, |tab| *tab == CoreTab::Projects);
    dock
}

fn to_tabs(node: &SavedNode, files: &mut FileTabs) -> SavedNode<CoreTab> {
    match node {
        SavedNode::Split {
            direction,
            fraction,
            first,
            second,
        } => SavedNode::Split {
            direction: *direction,
            fraction: *fraction,
            first: Box::new(to_tabs(first, files)),
            second: Box::new(to_tabs(second, files)),
        },
        SavedNode::Tabs {
            tabs,
            active,
            collapsed,
        } => SavedNode::Tabs {
            tabs: tabs.iter().map(|tab| tab.to_tab(files)).collect(),
            active: *active,
            collapsed: *collapsed,
        },
    }
}

fn leftmost(node: &SavedNode<CoreTab>) -> Vec<CoreTab> {
    match node {
        SavedNode::Split { first, .. } => leftmost(first),
        SavedNode::Tabs { tabs, .. } => tabs.clone(),
    }
}

fn populate(tree: &mut Tree<CoreTab>, index: NodeIndex, node: &SavedNode<CoreTab>) {
    match node {
        SavedNode::Split {
            direction,
            fraction,
            first,
            second,
        } => {
            let split = match direction {
                SplitDirection::Horizontal => Split::Right,
                SplitDirection::Vertical => Split::Below,
            };
            let [old, new] = tree.split(index, split, *fraction, Node::leaf_with(leftmost(second)));
            populate(tree, old, first);
            populate(tree, new, second);
        }
        SavedNode::Tabs {
            tabs,
            active,
            collapsed,
        } => {
            if let Ok(leaf) = tree.leaf_mut(index) {
                leaf.tabs.clone_from(tabs);
                leaf.active = TabIndex((*active).min(tabs.len().saturating_sub(1)));
                leaf.collapsed = *collapsed;
            }
        }
    }
}

/// Where the floating windows are. `egui_dock` 0.21 never fills in
/// [`egui_dock::WindowState::rect`], so each window's rect is read from egui's memory of
/// its area once the dock is drawn. A folded or minimized window keeps its open rect.
#[derive(Debug, Default)]
pub struct WindowRects(HashMap<SurfaceIndex, WindowRect>);

impl WindowRects {
    pub fn track(&mut self, dock: &DockState<CoreTab>, ctx: &egui::Context) {
        let mut open = HashMap::new();
        for (surface, content) in dock.iter_surfaces_indexed() {
            let Surface::Window(tree, _) = content else {
                continue;
            };
            let folded = tree.root_node().is_some_and(Node::is_collapsed);
            // The id `egui_dock` gives the window's area.
            let id = egui::Id::from(format!("window {surface:?}"));
            let rect = ctx
                .memory(|memory| memory.area_rect(id))
                .filter(|rect| !folded && rect.width().min(rect.height()) >= MIN_WINDOW_EDGE)
                .map(|rect| WindowRect {
                    x: rect.min.x,
                    y: rect.min.y,
                    width: rect.width(),
                    height: rect.height(),
                })
                .or_else(|| self.0.get(&surface).copied());
            if let Some(rect) = rect {
                open.insert(surface, rect);
            }
        }
        self.0 = open;
    }
}

/// The layout of `dock` as the file keeps it. Run tabs are left out, and a leaf of only
/// run tabs with them; `None` when the main surface has nothing else.
pub fn extract(dock: &DockState<CoreTab>, files: &FileTabs, rects: &WindowRects) -> Option<Layout> {
    let mut main = None;
    let mut windows = Vec::new();
    for (surface, content) in dock.iter_surfaces_indexed() {
        let root = NodePath::new(surface, NodeIndex::root());
        match content {
            Surface::Main(_) => main = extract_node(dock, root, files),
            Surface::Window(..) => {
                if let Some(root) = extract_node(dock, root, files) {
                    windows.push(SavedWindow {
                        rect: rects.0.get(&surface).copied(),
                        root,
                    });
                }
            }
            Surface::Empty => {}
        }
    }
    Some(Layout {
        main: main?,
        windows,
    })
}

fn extract_node(dock: &DockState<CoreTab>, path: NodePath, files: &FileTabs) -> Option<SavedNode> {
    match dock.node(path).ok()? {
        Node::Empty => None,
        Node::Leaf(leaf) => {
            let mut tabs = Vec::new();
            // The kept tab in front, or the nearest kept one before it.
            let mut active = 0;
            for (index, tab) in leaf.tabs.iter().enumerate() {
                let Some(saved) = SavedTab::of(*tab, files) else {
                    continue;
                };
                if index <= leaf.active.0 {
                    active = tabs.len();
                }
                tabs.push(saved);
            }
            (!tabs.is_empty()).then_some(SavedNode::Tabs {
                tabs,
                active,
                collapsed: leaf.collapsed,
            })
        }
        node @ (Node::Horizontal(split) | Node::Vertical(split)) => {
            let direction = if matches!(node, Node::Horizontal(_)) {
                SplitDirection::Horizontal
            } else {
                SplitDirection::Vertical
            };
            let first = extract_node(dock, child_path(path, true), files);
            let second = extract_node(dock, child_path(path, false), files);
            match (first, second) {
                (Some(first), Some(second)) => Some(SavedNode::Split {
                    direction,
                    fraction: split
                        .fraction
                        .clamp(MIN_SAVED_FRACTION, 1.0 - MIN_SAVED_FRACTION),
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (one, None) | (None, one) => one,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::dock::MIN_SIDEBAR_FRACTION;

    fn remove(dock: &mut DockState<CoreTab>, tab: CoreTab) {
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        dock.remove_tab(path);
    }

    fn main_root(dock: &DockState<CoreTab>) -> &Node<CoreTab> {
        dock.node(NodePath::new(SurfaceIndex::main(), NodeIndex::root()))
            .unwrap()
    }

    fn no_rects() -> WindowRects {
        WindowRects::default()
    }

    fn default_layout() -> Layout {
        extract(&shell::default_dock(), &FileTabs::default(), &no_rects()).unwrap()
    }

    fn store_with(dir: &Path, bytes: &[u8]) -> ClientStateStore {
        let store = ClientStateStore::in_config_dir(dir);
        std::fs::write(store.path(), bytes).unwrap();
        store
    }

    fn load_layout(store: &ClientStateStore) -> Option<Layout> {
        store.load().and_then(|state| state.layout)
    }

    fn tab(kind: &str) -> Value {
        json!({"kind": kind})
    }

    fn feature(name: &str) -> Value {
        json!({"kind": "feature", "feature": name})
    }

    fn tabs(tabs: Vec<Value>) -> Value {
        json!({"type": "tabs", "tabs": tabs})
    }

    fn split(direction: &str, fraction: f32, first: Value, second: Value) -> Value {
        json!({"type": "split", "direction": direction, "fraction": fraction,
               "first": first, "second": second})
    }

    fn file(main: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"version": 1, "layout": {"main": main}})).unwrap()
    }

    fn required(fraction: f32) -> Value {
        split(
            "horizontal",
            fraction,
            tabs(vec![tab("projects")]),
            tabs(vec![tab("conversation"), tab("notifications")]),
        )
    }

    // Given: the default dock with Lanes closed, Teams in front, a wider sidebar, a
    //        File tab beside Execution and the Terminal folded; run tabs and a selection
    // When: it is saved, loaded and restored into a fresh file registry
    // Then: the restored dock extracts to the same layout, the file is registered and
    //       the Terminal stays folded; the run tabs and the selection come back
    #[test]
    fn a_saved_layout_restores_the_same_dock() {
        let dir = tempfile::tempdir().unwrap();
        let mut dock = shell::default_dock();
        remove(&mut dock, CoreTab::Feature(FeatureId::Lanes));
        let teams = dock.find_tab(&CoreTab::Feature(FeatureId::Teams)).unwrap();
        dock.set_active_tab(teams).unwrap();
        if let Ok(Node::Horizontal(split)) =
            dock.node_mut(NodePath::new(SurfaceIndex::main(), NodeIndex::root()))
        {
            split.fraction = 0.42;
        }
        let mut files = FileTabs::default();
        let path = dir.path().join("src").join("lib.rs");
        let id = files.open(path.clone());
        shell::place_beside(
            &mut dock,
            CoreTab::File(id),
            CoreTab::Feature(FeatureId::Execution),
        );
        let layout = extract(&dock, &files, &no_rects()).unwrap();
        let run = RunPane {
            thread: ThreadId::new("thr-1").unwrap(),
            run: RunId::new("run-1a").unwrap(),
            name: "worker".into(),
            child: true,
        };
        let selection = SavedSelection {
            project: ProjectId::new("proj-1").unwrap(),
            thread: Some(ThreadId::new("thr-1").unwrap()),
        };
        let store = ClientStateStore::in_config_dir(&dir.path().join("config"));
        store
            .save(&SavedState {
                layout: Some(layout.clone()),
                run_tabs: vec![run.clone()],
                selection: Some(selection.clone()),
                folded_threads: vec![ThreadId::new("thr-2").unwrap()],
                opened_lanes: vec![ThreadId::new("thr-3").unwrap()],
            })
            .unwrap();

        let loaded = store.load().unwrap();
        let mut fresh = FileTabs::default();
        let restored = restore_dock(loaded.layout.as_ref(), &mut fresh);

        assert_eq!(extract(&restored, &fresh, &no_rects()), Some(layout));
        assert_eq!(
            (
                loaded.run_tabs,
                loaded.selection,
                loaded.folded_threads,
                loaded.opened_lanes
            ),
            (
                vec![run],
                Some(selection),
                vec![ThreadId::new("thr-2").unwrap()],
                vec![ThreadId::new("thr-3").unwrap()]
            )
        );
        let (_, file) = restored
            .iter_all_tabs()
            .find(|(_, tab)| matches!(tab, CoreTab::File(_)))
            .unwrap();
        let CoreTab::File(id) = *file else {
            unreachable!()
        };
        assert_eq!(fresh.get(id).unwrap().path, path);
        let terminal = restored.find_tab(&CoreTab::Terminal).unwrap();
        assert!(restored.leaf(terminal.node_path()).unwrap().collapsed);
        assert!(
            restored
                .find_tab(&CoreTab::Feature(FeatureId::Lanes))
                .is_none()
        );
    }

    // Given: a leaf with an unknown kind, an unknown feature, the workspace feature, a
    //        second Conversation, a file path twice and a relative one, Teams in front
    // When: the file is loaded
    // Then: those tabs are dropped, the others are kept in order, and Teams stays in front
    #[test]
    fn unknown_and_repeated_tabs_are_dropped_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let path_tab = json!({"kind": "file", "path": path});
        let second = json!({"type": "tabs", "active": 9, "tabs": [
            tab("conversation"), tab("bogus"), feature("nonsense"), feature("workspace"),
            tab("conversation"), path_tab, path_tab, {"kind": "file", "path": "relative.rs"},
            tab("notifications"), feature("teams"),
        ]});
        let main = split(
            "horizontal",
            0.3,
            tabs(vec![tab("projects"), json!(42)]),
            second,
        );
        let store = store_with(dir.path(), &file(main));

        let layout = load_layout(&store).unwrap();

        let SavedNode::Split { first, second, .. } = layout.main else {
            panic!("a split")
        };
        assert_eq!(
            *first,
            SavedNode::Tabs {
                tabs: vec![SavedTab::Projects],
                active: 0,
                collapsed: false
            }
        );
        assert_eq!(
            *second,
            SavedNode::Tabs {
                tabs: vec![
                    SavedTab::Conversation,
                    SavedTab::File { path },
                    SavedTab::Notifications,
                    SavedTab::Feature {
                        feature: FeatureId::Teams
                    },
                ],
                active: 3,
                collapsed: false
            }
        );
    }

    // Given: a vertical split whose lower leaf has only tabs that are dropped
    // When: the file is loaded
    // Then: the leaf goes with its split, and the upper leaf takes its place
    #[test]
    fn a_leaf_left_empty_goes_with_its_split() {
        let dir = tempfile::tempdir().unwrap();
        let main = split(
            "horizontal",
            0.3,
            tabs(vec![tab("projects")]),
            split(
                "vertical",
                0.5,
                tabs(vec![tab("conversation"), tab("notifications")]),
                tabs(vec![tab("bogus"), feature("workspace")]),
            ),
        );
        let store = store_with(dir.path(), &file(main));

        let layout = load_layout(&store).unwrap();

        let expected: Layout<Value> =
            serde_json::from_value(json!({"main": required(0.3)})).unwrap();
        assert_eq!(Some(layout), expected.validate().ok());
    }

    // Given: files that cannot be read, decoded or used as a layout
    // When: each is loaded and restored
    // Then: the default dock comes back, and nothing panics
    #[test]
    fn a_bad_file_restores_the_default_dock() {
        let window = |rect: Value| {
            serde_json::to_vec(&json!({"version": 1, "layout": {
                "main": required(0.3),
                "windows": [{"rect": rect, "root": tabs(vec![tab("plan")])}],
            }}))
            .unwrap()
        };
        let cases: Vec<(&str, Option<Vec<u8>>)> = vec![
            ("missing", None),
            ("empty", Some(Vec::new())),
            (
                "truncated",
                Some(br#"{"version":1,"layout":{"main":"#.to_vec()),
            ),
            ("array", Some(b"[]".to_vec())),
            (
                "version 2",
                Some(
                    serde_json::to_vec(&json!({"version": 2, "layout": {"main": required(0.3)}}))
                        .unwrap(),
                ),
            ),
            ("fraction 0", Some(file(required(0.0)))),
            ("fraction 1", Some(file(required(1.0)))),
            ("fraction 1.5", Some(file(required(1.5)))),
            (
                "no conversation",
                Some(file(split(
                    "horizontal",
                    0.3,
                    tabs(vec![tab("projects")]),
                    tabs(vec![tab("notifications")]),
                ))),
            ),
            ("too large", Some(vec![b' '; MAX_BYTES as usize + 1])),
            (
                "not utf-8",
                Some(b"{\"version\":1,\"layout\":\"\xff\xfe\"}".to_vec()),
            ),
            (
                "bad window",
                Some(window(
                    json!({"x": 0.0, "y": 0.0, "width": -1.0, "height": 10.0}),
                )),
            ),
        ];
        for (name, bytes) in cases {
            let dir = tempfile::tempdir().unwrap();
            let store = match bytes {
                Some(bytes) => store_with(dir.path(), &bytes),
                None => ClientStateStore::in_config_dir(dir.path()),
            };
            let layout = load_layout(&store);
            assert_eq!(layout, None, "{name}");
            let mut files = FileTabs::default();
            let dock = restore_dock(layout.as_ref(), &mut files);
            assert_eq!(
                extract(&dock, &files, &no_rects()),
                Some(default_layout()),
                "{name}"
            );
        }
    }

    /// How deep the deepest leaf of `node` is.
    fn depth(node: &SavedNode) -> usize {
        match node {
            SavedNode::Split { first, second, .. } => 1 + depth(first).max(depth(second)),
            SavedNode::Tabs { .. } => 0,
        }
    }

    // Given: a file whose folded threads hold a bad entry and a section of the wrong type
    // When: it is decoded / Then: the good thread ids are kept, and a wrong section reads as none
    #[test]
    fn folded_threads_that_do_not_decode_are_dropped() {
        let bytes =
            serde_json::to_vec(&json!({"version": 1, "folded_threads": ["thr-1", 7, "", "thr-2"]}))
                .unwrap();
        let state = SavedState::decode(&bytes).unwrap();
        assert_eq!(
            state.folded_threads,
            [
                ThreadId::new("thr-1").unwrap(),
                ThreadId::new("thr-2").unwrap()
            ]
        );
        let bytes = serde_json::to_vec(&json!({"version": 1, "folded_threads": "x"})).unwrap();
        assert!(
            SavedState::decode(&bytes)
                .unwrap()
                .folded_threads
                .is_empty()
        );
    }

    // Given: the default dock with its splits dragged to the edges, at fraction 0 and 1
    // When: it is extracted, saved and loaded
    // Then: the layout loads, each split kept just inside the edge it was dragged to
    #[test]
    fn a_split_dragged_to_an_edge_saves_a_layout_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let mut dock = shell::default_dock();
        let mut edge = 0.0;
        for (_, node) in dock.iter_all_nodes_mut() {
            if let Node::Horizontal(split) | Node::Vertical(split) = node {
                split.fraction = edge;
                edge = 1.0 - edge;
            }
        }
        let store = ClientStateStore::in_config_dir(dir.path());
        store
            .save(&SavedState {
                layout: extract(&dock, &FileTabs::default(), &no_rects()),
                ..SavedState::default()
            })
            .unwrap();

        let layout = load_layout(&store).expect("the saved layout loads");

        let mut fractions = Vec::new();
        let mut nodes = vec![&layout.main];
        while let Some(node) = nodes.pop() {
            if let SavedNode::Split {
                fraction,
                first,
                second,
                ..
            } = node
            {
                fractions.push(*fraction);
                nodes.extend([&**first, &**second]);
            }
        }
        assert!(fractions.contains(&MIN_SAVED_FRACTION), "{fractions:?}");
        assert!(
            fractions.contains(&(1.0 - MIN_SAVED_FRACTION)),
            "{fractions:?}"
        );
    }

    // Given: a dock with a leaf of 300 File tabs and, below it, 12 nested splits whose
    //        deepest leaf holds the Conversation and Notifications
    // When: it is extracted, saved, loaded and restored
    // Then: the layout loads; past the tab budget only the required tabs are kept, and the
    //       splits past the depth limit are merged into one leaf with all their tabs
    #[test]
    fn a_dock_past_the_limits_saves_a_layout_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let file_tab = |name: String| SavedTab::File {
            path: dir.path().join(name),
        };
        let leaf = |tabs: Vec<SavedTab>| SavedNode::Tabs {
            tabs,
            active: 0,
            collapsed: false,
        };
        let vertical = |first: SavedNode, second: SavedNode| SavedNode::Split {
            direction: SplitDirection::Vertical,
            fraction: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };
        let mut deep = leaf(vec![SavedTab::Conversation, SavedTab::Notifications]);
        for level in 0..12 {
            deep = vertical(leaf(vec![file_tab(format!("deep-{level}.rs"))]), deep);
        }
        let many = leaf((0..300).map(|n| file_tab(format!("{n}.rs"))).collect());
        let built = Layout {
            main: SavedNode::Split {
                direction: SplitDirection::Horizontal,
                fraction: 0.3,
                first: Box::new(leaf(vec![SavedTab::Projects])),
                second: Box::new(vertical(many, deep)),
            },
            windows: Vec::new(),
        };
        let mut files = FileTabs::default();
        let dock = restore_dock(Some(&built), &mut files);
        let store = ClientStateStore::in_config_dir(dir.path());
        store
            .save(&SavedState {
                layout: extract(&dock, &files, &no_rects()),
                ..SavedState::default()
            })
            .unwrap();

        let layout = load_layout(&store).expect("the saved layout loads");

        assert!(depth(&layout.main) <= MAX_DEPTH);
        let restored = restore_dock(Some(&layout), &mut FileTabs::default());
        let tabs: Vec<CoreTab> = restored.iter_all_tabs().map(|(_, tab)| *tab).collect();
        assert_eq!(tabs.len(), MAX_TABS + 2);
        for tab in [
            CoreTab::Projects,
            CoreTab::Conversation,
            CoreTab::Notifications,
        ] {
            assert!(tabs.contains(&tab), "{tab:?}");
        }
    }

    // Given: floating windows saved at a usable rect, at a folded window's height, and
    //        without a rect
    // When: the file is loaded
    // Then: only the usable rect is kept; the others open at egui's default size
    #[test]
    fn a_window_rect_too_small_to_show_the_window_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let window = |rect: Value, kind: &str| json!({"rect": rect, "root": tabs(vec![tab(kind)])});
        let usable = json!({"x": 200.0, "y": 150.0, "width": 400.0, "height": 300.0});
        let folded = json!({"x": 200.0, "y": 150.0, "width": 400.0, "height": 30.0});
        let bytes = serde_json::to_vec(&json!({"version": 1, "layout": {
            "main": required(0.3),
            "windows": [
                window(usable.clone(), "plan"),
                window(folded, "diff"),
                {"root": tabs(vec![tab("terminal")])},
            ],
        }}))
        .unwrap();
        let store = store_with(dir.path(), &bytes);

        let layout = load_layout(&store).unwrap();

        let rects: Vec<_> = layout.windows.iter().map(|window| window.rect).collect();
        assert_eq!(
            rects,
            [Some(serde_json::from_value(usable).unwrap()), None, None]
        );
    }

    // Given: a file another build wrote at version 2
    // When: it is loaded, then saved on exit, then saved explicitly
    // Then: the exit save leaves its bytes as they are; the explicit save replaces it
    #[test]
    fn a_file_that_cannot_be_read_is_replaced_only_by_an_explicit_save() {
        let dir = tempfile::tempdir().unwrap();
        let bytes =
            serde_json::to_vec(&json!({"version": 2, "layout": {"main": required(0.3)}})).unwrap();
        let store = store_with(dir.path(), &bytes);
        assert!(store.load().is_none());

        assert!(!store.autosave(&SavedState::default()).unwrap());
        assert_eq!(std::fs::read(store.path()).unwrap(), bytes);

        store.save(&SavedState::default()).unwrap();
        assert!(store.load().is_some());
        assert!(store.autosave(&SavedState::default()).unwrap());
    }

    // Given: a layout whose projects take a tenth of the width
    // When: it is restored
    // Then: the projects get the sidebar's minimum share
    #[test]
    fn a_narrow_sidebar_is_widened_on_restore() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path(), &file(required(0.1)));

        let dock = restore_dock(load_layout(&store).as_ref(), &mut FileTabs::default());

        let Node::Horizontal(split) = main_root(&dock) else {
            panic!("a horizontal root")
        };
        assert_eq!(split.fraction, MIN_SIDEBAR_FRACTION);
    }

    // Given: a store whose directory is a file
    // When: a state is saved
    // Then: it fails without panicking
    #[test]
    fn saving_where_the_directory_is_a_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("config");
        std::fs::write(&blocker, b"").unwrap();
        let store = ClientStateStore::in_config_dir(&blocker);

        assert!(store.save(&SavedState::default()).is_err());
    }

    /// The names the file keeps the features by. They must not change, or saved layouts
    /// lose their tabs.
    const PINNED: [(FeatureId, &str); 16] = [
        (FeatureId::Workspace, "workspace"),
        (FeatureId::Mcp, "mcp"),
        (FeatureId::Execution, "execution"),
        (FeatureId::Teams, "teams"),
        (FeatureId::Lanes, "lanes"),
        (FeatureId::Settings, "settings"),
        (FeatureId::Auth, "auth"),
        (FeatureId::Diagnostics, "diagnostics"),
        (FeatureId::Memory, "memory"),
        (FeatureId::Improvement, "improvement"),
        (FeatureId::Evaluation, "evaluation"),
        (FeatureId::PullRequests, "pull_requests"),
        (FeatureId::Maintenance, "maintenance"),
        (FeatureId::Artifacts, "artifacts"),
        (FeatureId::Browser, "browser"),
        (FeatureId::Restart, "restart"),
    ];

    /// A new feature fails to compile here until it has a place in [`PINNED`].
    fn pinned_index(feature: FeatureId) -> usize {
        match feature {
            FeatureId::Workspace => 0,
            FeatureId::Mcp => 1,
            FeatureId::Execution => 2,
            FeatureId::Teams => 3,
            FeatureId::Lanes => 4,
            FeatureId::Settings => 5,
            FeatureId::Auth => 6,
            FeatureId::Diagnostics => 7,
            FeatureId::Memory => 8,
            FeatureId::Improvement => 9,
            FeatureId::Evaluation => 10,
            FeatureId::PullRequests => 11,
            FeatureId::Maintenance => 12,
            FeatureId::Artifacts => 13,
            FeatureId::Browser => 14,
            FeatureId::Restart => 15,
        }
    }

    #[test]
    fn feature_names_in_the_file_are_pinned() {
        for (index, (feature, name)) in PINNED.into_iter().enumerate() {
            assert_eq!(pinned_index(feature), index, "{name}");
            assert_eq!(serde_json::to_value(feature).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<FeatureId>(json!(name)).unwrap(),
                feature
            );
        }
    }
}
