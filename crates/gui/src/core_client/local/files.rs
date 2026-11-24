//! The File tabs of the core client: one closeable, read-only preview per local file,
//! opened from a link in a transcript or in another File tab. As in the embedded
//! workbench, a path has one tab, and the tabs stay while other threads are selected.
//! The dock holds only their ids, so `CoreTab` stays `Copy`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use egui_dock::DockState;

use crate::core_client::shell::CoreTab;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileTabId(u32);

#[derive(Debug, Clone)]
pub struct FileTab {
    pub path: PathBuf,
    pub title: String,
}

#[derive(Default)]
pub struct FileTabs {
    next: u32,
    tabs: BTreeMap<FileTabId, FileTab>,
}

impl FileTabs {
    /// The tab of `path`, reusing the one already open for it. It is titled with the
    /// file's name, or its whole path when it has none.
    pub fn open(&mut self, path: PathBuf) -> FileTabId {
        if let Some((id, _)) = self.tabs.iter().find(|(_, tab)| tab.path == path) {
            return *id;
        }
        let title = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.next += 1;
        let id = FileTabId(self.next);
        self.tabs.insert(id, FileTab { path, title });
        id
    }

    pub fn get(&self, id: FileTabId) -> Option<&FileTab> {
        self.tabs.get(&id)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Drops the tabs the dock lost (closed, or a reset layout); returns their paths.
    pub fn sweep(&mut self, dock: &DockState<CoreTab>) -> Vec<PathBuf> {
        let mut gone = Vec::new();
        self.tabs.retain(|id, tab| {
            let kept = dock.find_tab(&CoreTab::File(*id)).is_some();
            if !kept {
                gone.push(std::mem::take(&mut tab.path));
            }
            kept
        });
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reuses_the_tab_of_a_path_and_titles_it_with_the_file_name() {
        let mut tabs = FileTabs::default();
        let source = tabs.open(PathBuf::from("/work/src/lib.rs"));
        assert_eq!(tabs.open(PathBuf::from("/work/src/lib.rs")), source);
        assert_eq!(tabs.get(source).unwrap().title, "lib.rs");
        let other = tabs.open(PathBuf::from("/work/src/main.rs"));
        assert_ne!(other, source);
        let root = tabs.open(PathBuf::from("/"));
        assert_eq!(tabs.get(root).unwrap().title, "/");
    }

    // Given: two file tabs in the dock
    // When: one is closed and the tabs are swept twice
    // Then: the closed tab's path is returned once and the other tab is kept
    #[test]
    fn sweep_returns_a_closed_tabs_path_once() {
        let mut tabs = FileTabs::default();
        let kept = tabs.open(PathBuf::from("/work/a.rs"));
        let closed = tabs.open(PathBuf::from("/work/b.rs"));
        let mut dock = DockState::new(vec![
            CoreTab::Conversation,
            CoreTab::File(kept),
            CoreTab::File(closed),
        ]);
        let path = dock.find_tab(&CoreTab::File(closed)).unwrap();
        dock.remove_tab(path);

        assert_eq!(tabs.sweep(&dock), [PathBuf::from("/work/b.rs")]);
        assert!(tabs.sweep(&dock).is_empty());
        assert!(tabs.get(kept).is_some() && tabs.get(closed).is_none());
    }

    // Given: two file tabs
    // When: the layout is reset to a dock without them
    // Then: both paths are returned and the registry is empty
    #[test]
    fn a_fresh_dock_drops_every_tab() {
        let mut tabs = FileTabs::default();
        tabs.open(PathBuf::from("/work/a.rs"));
        tabs.open(PathBuf::from("/work/b.rs"));
        let dock = DockState::new(vec![CoreTab::Conversation]);

        let mut gone = tabs.sweep(&dock);
        gone.sort();
        assert_eq!(
            gone,
            [PathBuf::from("/work/a.rs"), PathBuf::from("/work/b.rs")]
        );
        assert!(tabs.is_empty());
    }
}
