//! The panes that work on this machine's files and processes: the Diff of the selected
//! thread's working tree, one shell per project and the previews of linked files
//! ([`files`]), drawn with the embedded workbench's panes. The core client runs beside
//! the core, so the directories the core reports are read here directly ([`context`]);
//! nothing of this goes through the core.
pub mod context;
pub mod files;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use workbench_protocol::ids::ProjectId;
use workbench_protocol::views::ProjectSummary;

pub use context::{LocalRoot, LocalRoots};

use crate::diff::{AUTO_REFRESH_INTERVAL, DiffModel, DiffRequest, DiffSource, GitCliDiffSource};
use crate::panes::diff::{diff_pane, selected_mode};
use crate::panes::file_viewer::{self, FileLink, file_viewer_pane, resolve_file_link};
use crate::panes::terminal::terminal_pane;
use crate::pty::{TerminalSpawner, resolve_terminal_cwd};
use crate::terminal::TerminalSessionsOf;
use crate::theme::widgets::{empty_state, spinner};

use super::shell::CoreTab;

/// The Terminal's shells are keyed by project; `None` is the shell in the home directory.
pub type LocalTerminalKey = Option<ProjectId>;

/// How often a frame looks for the answer of a diff fetch, whose worker cannot wake it.
const DIFF_POLL: Duration = Duration::from_millis(100);

pub struct LocalPanes {
    pub(super) diff: DiffModel,
    diff_source: Arc<dyn DiffSource>,
    pub(super) terminals: TerminalSessionsOf<LocalTerminalKey>,
    home: PathBuf,
    roots: LocalRoots,
    pub(super) files: files::FileTabs,
}

impl Default for LocalPanes {
    fn default() -> Self {
        Self::new(Arc::new(GitCliDiffSource), None)
    }
}

impl LocalPanes {
    pub fn new(
        diff_source: Arc<dyn DiffSource>,
        spawner: Option<Arc<dyn TerminalSpawner>>,
    ) -> Self {
        let mut terminals = TerminalSessionsOf::default();
        if let Some(spawner) = spawner {
            terminals.set_spawner(spawner);
        }
        let home = std::env::home_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(std::env::temp_dir);
        Self {
            diff: DiffModel::new(),
            diff_source,
            terminals,
            home,
            roots: LocalRoots::default(),
            files: files::FileTabs::default(),
        }
    }

    /// Where a shell starts when no project root is known.
    #[must_use]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = home;
        self
    }

    /// Before the dock: follow the selection's roots, collect what the diff worker and
    /// the shells produced, and end the shells of projects the core no longer lists
    /// (`projects`, the sidebar's last complete listing, given only while connected).
    /// The selected project's shell is kept: a project can be selected before a
    /// listing that names it completes, and its pane would start a shell every frame.
    pub fn begin_frame(
        &mut self,
        ctx: &egui::Context,
        fresh: LocalRoots,
        projects: Option<&[ProjectSummary]>,
    ) {
        self.roots = fresh.keep_known(&self.roots);
        self.diff
            .set_repo_root(self.roots.workspace.ready().map(Path::to_path_buf));
        self.diff.poll();
        if self.diff.is_busy() {
            ctx.request_repaint_after(DIFF_POLL);
        }
        // The terminal pane claims input again while it is drawn in this frame.
        self.terminals.focused = false;
        self.terminals.grid_rect = None;
        if let Some(projects) = projects {
            let selected = self.roots.project.as_ref();
            self.terminals.retain_projects(|id| {
                selected == Some(id) || projects.iter().any(|project| project.id == *id)
            });
        }
        if self.terminals.poll() {
            ctx.request_repaint();
        }
    }

    pub fn diff_tab(&mut self, ui: &mut egui::Ui) {
        let root = match &self.roots.workspace {
            LocalRoot::None => None,
            LocalRoot::Pending => return loading(ui),
            LocalRoot::Missing(path) => return not_available(ui, path),
            LocalRoot::Ready(path) => Some(path.clone()),
        };
        let requested = diff_pane(ui, &self.diff);
        let Some(repo_root) = root else {
            return;
        };
        let source = Arc::clone(&self.diff_source);
        match requested {
            Some(mode) => self.diff.request(source, DiffRequest { repo_root, mode }),
            None => self.diff.refresh_if_due(
                source,
                DiffRequest {
                    repo_root,
                    mode: selected_mode(ui),
                },
                Instant::now(),
            ),
        }
        if !self.diff.is_snapshot() {
            ui.ctx().request_repaint_after(AUTO_REFRESH_INTERVAL);
        }
    }

    /// The selected project's shell. A shell starts in the root it is created with, so
    /// none is created before that root is known.
    pub fn terminal_tab(&mut self, ui: &mut egui::Ui) {
        let key = self.roots.project.clone();
        if self.terminals.get(&key).is_none() {
            match &self.roots.project_root {
                LocalRoot::Pending => return loading(ui),
                LocalRoot::Missing(path) => return not_available(ui, path),
                LocalRoot::None | LocalRoot::Ready(_) => {}
            }
        }
        let cwd = resolve_terminal_cwd(self.roots.project_root.ready(), &self.home);
        terminal_pane(ui, &mut self.terminals, &key, &cwd);
    }

    /// The selected thread's working tree on this machine, which relative links in
    /// the transcripts resolve against.
    pub fn workspace(&self) -> Option<&Path> {
        self.roots.workspace.ready()
    }

    pub fn file_tab(&mut self, ui: &mut egui::Ui, id: files::FileTabId) {
        if let Some(tab) = self.files.get(id) {
            file_viewer_pane(ui, &tab.path, self.roots.workspace.ready());
        }
    }

    pub fn file_title(&self, id: files::FileTabId) -> Option<&str> {
        self.files.get(id).map(|tab| tab.title.as_str())
    }

    pub fn file_base(&self, id: files::FileTabId) -> Option<&Path> {
        self.files.get(id).and_then(|tab| tab.path.parent())
    }

    /// The tab of `link`'s file, which scrolls to the link's line when drawn.
    pub fn open_file(&mut self, ctx: &egui::Context, link: FileLink) -> files::FileTabId {
        file_viewer::request_line(ctx, &link);
        self.files.open(link.path)
    }

    /// After the dock: a File tab the dock lost (closed, or a reset layout) is dropped
    /// with its preview, so the file is read anew when opened again.
    pub fn sweep_files(&mut self, ctx: &egui::Context, dock: &egui_dock::DockState<CoreTab>) {
        for path in self.files.sweep(dock) {
            file_viewer::forget_file(ctx, &path);
        }
    }

    /// Before egui reads the input: a focused terminal takes the keys, so no shortcut
    /// and no focus move steals one the shell needs. The core client has no keymap,
    /// so no key passes through.
    pub fn capture_input(&mut self, raw: &mut egui::RawInput) {
        if !self.terminals.focused {
            return;
        }
        // An empty paste is the platform's image paste, never text for a shell.
        raw.events
            .retain(|event| !matches!(event, egui::Event::Paste(text) if text.is_empty()));
        self.terminals.capture_input(raw, |_, _| false);
    }
}

/// Takes the local-file links a tab opened since output command `start`, as the
/// embedded workbench does, resolving relative ones against `base`. Without a base, a
/// relative link is dropped too: the desktop would resolve it against this process's
/// working directory.
pub fn take_links(ctx: &egui::Context, start: usize, base: Option<&Path>) -> Vec<FileLink> {
    let links = file_viewer::take_file_links(ctx, start, base);
    if base.is_none() {
        let any_base = Some(Path::new("/"));
        ctx.output_mut(|output| {
            let mut index = 0;
            output.commands.retain(|command| {
                let eligible = index >= start;
                index += 1;
                !(eligible
                    && matches!(command, egui::OutputCommand::OpenUrl(url)
                        if resolve_file_link(&url.url, any_base).is_some()))
            });
        });
    }
    links
}

fn loading(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        spinner(ui);
        ui.label("Loading workspace…");
    });
}

fn not_available(ui: &mut egui::Ui, path: &Path) {
    empty_state(
        ui,
        "Workspace not available on this machine",
        &format!(
            "The core works in {}, which this machine does not have.",
            path.display()
        ),
        None,
    );
}

#[cfg(test)]
mod tests {
    use egui::{Event, Key, Modifiers, PointerButton, RawInput, pos2, vec2};

    use super::*;

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn batch() -> Vec<Event> {
        vec![
            Event::Text("a".into()),
            key(Key::Tab, Modifiers::NONE),
            key(Key::Num1, Modifiers::CTRL),
            Event::Paste(String::new()),
            Event::PointerButton {
                pos: pos2(500.0, 500.0),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::Copy,
        ]
    }

    // Given: a terminal that held focus in the last frame
    // When: keys, an empty paste, a press outside its grid and a copy arrive in one batch
    // Then: the keys before the press go to the terminal, shortcuts included, the empty
    //       paste is dropped, and the press and what follows stay with egui
    #[test]
    fn a_focused_terminal_takes_the_keys_until_a_press_outside_its_grid() {
        let mut local = LocalPanes::new(Arc::new(crate::diff::FixtureDiffSource::empty()), None);
        local.terminals.focused = true;
        local.terminals.grid_rect = Some(egui::Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(100.0, 100.0),
        ));
        let mut raw = RawInput {
            events: batch(),
            ..RawInput::default()
        };

        local.capture_input(&mut raw);

        let expected = batch();
        assert_eq!(local.terminals.pending_events, expected[..3]);
        assert_eq!(raw.events, expected[4..]);
    }

    /// Records where each shell would start, and starts none.
    #[derive(Default)]
    struct RecordingSpawner(std::sync::Mutex<Vec<PathBuf>>);

    impl TerminalSpawner for RecordingSpawner {
        fn spawn(
            &self,
            cwd: &Path,
            _: u16,
            _: u16,
            _: crate::pty::Wake,
        ) -> Result<crate::pty::PtySession, crate::pty::TerminalError> {
            self.0.lock().unwrap().push(cwd.to_path_buf());
            Err(crate::pty::TerminalError::Spawn("test spawner".into()))
        }
    }

    // Given: proj-2 selected with its root on this machine, but missing from the core's
    //        last complete listing (a relist failed or has not answered yet)
    // When: frames with the Terminal drawn keep coming
    // Then: proj-2's shell is started once and kept
    #[test]
    fn the_selected_projects_shell_outlives_a_listing_that_lacks_it() {
        let root = tempfile::tempdir().unwrap();
        let spawner = Arc::new(RecordingSpawner::default());
        let mut local = LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            Some(spawner.clone()),
        );
        let roots = LocalRoots {
            project: Some(ProjectId::new("proj-2").unwrap()),
            thread: None,
            project_root: LocalRoot::Ready(root.path().into()),
            workspace: LocalRoot::Ready(root.path().into()),
        };
        let ctx = egui::Context::default();

        for _ in 0..4 {
            let mut output = ctx.run_ui(RawInput::default(), |ui| {
                local.begin_frame(ui.ctx(), roots.clone(), Some(&[]));
                local.terminal_tab(ui);
            });
            output.textures_delta.clear();
        }

        assert_eq!(spawner.0.lock().unwrap().as_slice(), [root.path()]);
        assert_eq!(local.terminals.len(), 1);
    }

    #[test]
    fn an_unfocused_terminal_leaves_the_input_alone() {
        let mut local = LocalPanes::new(Arc::new(crate::diff::FixtureDiffSource::empty()), None);
        let mut raw = RawInput {
            events: batch(),
            ..RawInput::default()
        };

        local.capture_input(&mut raw);

        assert!(local.terminals.pending_events.is_empty());
        assert_eq!(raw.events, batch());
    }
}
