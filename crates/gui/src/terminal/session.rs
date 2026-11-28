//! プロジェクトごとの端末セッションです。
//!
//! セッションは Terminal ペインが初めて表示したときに起動し、プロジェクトを切り替えても
//! 背景で動き続けます。シェルが終了したら終了コードを保持し、再起動を受け付けます。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::emulator::{GridSize, TerminalEmulator, ViewportPoint};
use super::input;
use crate::pty::{PtySession, TerminalSpawner, Wake};

/// シェルプロセスの状態です。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStatus {
    /// 起動前、または再起動待ちです。
    NotStarted,
    Running,
    Exited(u32),
    Failed(String),
}

/// 1 つのシェルと、その画面状態です。
pub struct TerminalSession {
    emulator: TerminalEmulator,
    pty: Option<PtySession>,
    status: TerminalStatus,
    cwd: PathBuf,
    focused: bool,
    /// ホイールの端数 (1 行未満のスクロール量) です。
    pub(crate) scroll_remainder: f32,
    /// マウス報告でドラッグ中に最後に通知したセルです。
    pub(crate) last_drag_cell: Option<ViewportPoint>,
}

impl TerminalSession {
    fn new(cwd: PathBuf) -> Self {
        Self {
            emulator: TerminalEmulator::new(GridSize::DEFAULT),
            pty: None,
            status: TerminalStatus::NotStarted,
            cwd,
            focused: false,
            scroll_remainder: 0.0,
            last_drag_cell: None,
        }
    }

    pub const fn emulator(&self) -> &TerminalEmulator {
        &self.emulator
    }

    pub const fn emulator_mut(&mut self) -> &mut TerminalEmulator {
        &mut self.emulator
    }

    pub const fn status(&self) -> &TerminalStatus {
        &self.status
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 未起動ならシェルを起動します。
    pub fn ensure_started(&mut self, spawner: &dyn TerminalSpawner, wake: Wake) {
        if self.status != TerminalStatus::NotStarted {
            return;
        }
        let size = self.emulator.size();
        match spawner.spawn(&self.cwd, size.lines, size.columns, wake) {
            Ok(pty) => {
                self.pty = Some(pty);
                self.status = TerminalStatus::Running;
            }
            Err(error) => self.status = TerminalStatus::Failed(error.to_string()),
        }
    }

    /// 受信済みの出力を画面へ反映し、端末の応答を書き戻します。画面か状態が変化したら `true`。
    ///
    /// 終了後も PTY は再起動まで保持し、終了直前に書かれた出力を取りこぼさないようにします。
    pub fn poll(&mut self) -> bool {
        let Some(pty) = &mut self.pty else {
            return false;
        };
        let output = pty.drain_output();
        let mut changed = !output.is_empty();
        if changed {
            self.emulator.feed(&output);
            let responses = self.emulator.take_responses();
            if !responses.is_empty() && self.status == TerminalStatus::Running {
                let _ = pty.write(&responses);
            }
        }
        if self.status == TerminalStatus::Running
            && let Some(code) = pty.exit_code()
        {
            self.status = TerminalStatus::Exited(code);
            changed = true;
        }
        changed
    }

    /// ユーザー入力を PTY へ送ります。表示を最下部へ戻し、選択を解除します。
    pub fn write_input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.status == TerminalStatus::Running
            && let Some(pty) = &mut self.pty
        {
            let _ = pty.write(bytes);
        }
        self.emulator.clear_selection();
        self.emulator.scroll_to_bottom();
    }

    /// アプリケーションへ送る生のバイト列 (マウス報告など) です。表示位置は変えません。
    pub fn write_raw(&mut self, bytes: &[u8]) {
        if self.status == TerminalStatus::Running
            && let Some(pty) = &mut self.pty
        {
            let _ = pty.write(bytes);
        }
    }

    /// グリッドと PTY の大きさを合わせます。
    pub fn resize(&mut self, size: GridSize) {
        if self.emulator.resize(size)
            && let Some(pty) = &mut self.pty
        {
            let _ = pty.resize(size.lines, size.columns);
        }
    }

    /// フォーカス変化を記録し、アプリケーションが要求していれば通知します。
    pub fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        if let Some(bytes) = input::focus_bytes(focused, self.emulator.mode()) {
            self.write_raw(bytes);
        }
    }

    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// 現在のシェルを終了し、次の表示で新しいシェルを起動します。
    pub fn restart(&mut self) {
        self.pty = None;
        self.emulator.reset();
        self.status = TerminalStatus::NotStarted;
    }
}

/// すべての端末セッションと、キーボード入力の受け渡し状態です。
///
/// セッションの識別子 `K` はホストごとに異なります。レガシーの Workbench は
/// [`TerminalKey`]、core client はプロトコルのプロジェクト ID を使います。
pub struct TerminalSessionsOf<K> {
    sessions: BTreeMap<K, TerminalSession>,
    spawner: Option<Arc<dyn TerminalSpawner>>,
    /// 前フレームで端末がキーボードフォーカスを持っていたかどうかです。
    pub(crate) focused: bool,
    /// 前フレームでフォーカス中だった端末のグリッド領域です。`raw_input_hook` が、領域外の
    /// 押下以降のイベントを端末へ取り込まないために使います。
    pub(crate) grid_rect: Option<egui::Rect>,
    /// `raw_input_hook` が端末用に取り分けたキーボードイベントです。
    pub(crate) pending_events: Vec<egui::Event>,
}

impl<K> Default for TerminalSessionsOf<K> {
    fn default() -> Self {
        Self {
            sessions: BTreeMap::new(),
            spawner: None,
            focused: false,
            grid_rect: None,
            pending_events: Vec::new(),
        }
    }
}

impl<K> TerminalSessionsOf<K> {
    pub fn set_spawner(&mut self, spawner: Arc<dyn TerminalSpawner>) {
        self.spawner = Some(spawner);
    }

    pub fn spawner(&self) -> Option<Arc<dyn TerminalSpawner>> {
        self.spawner.clone()
    }

    /// Routes keyboard input to the focused terminal before egui sees it.
    ///
    /// Tab, arrows and Escape would otherwise move or drop focus, and global shortcuts
    /// would steal control keys the shell needs. Keys for which `passes_through` returns
    /// `true` (host shortcuts a terminal has no use for) still reach the host.
    ///
    /// A button press outside the terminal grid hands input ownership back: it and every
    /// later event of the batch stay with egui (so Ctrl+C right after it copies a selection),
    /// while earlier events still belong to the terminal.
    pub fn capture_input(
        &mut self,
        raw_input: &mut egui::RawInput,
        passes_through: impl Fn(egui::Key, egui::Modifiers) -> bool,
    ) {
        let pending = &mut self.pending_events;
        let grid = self.grid_rect;
        let mut released = false;
        raw_input.events.retain(|event| {
            if let egui::Event::PointerButton {
                pos, pressed: true, ..
            } = event
                && grid.is_some_and(|grid| !grid.contains(*pos))
            {
                released = true;
            }
            if released {
                return true;
            }
            let captured = match event {
                egui::Event::Key { key, modifiers, .. } => !passes_through(*key, *modifiers),
                egui::Event::Text(_)
                | egui::Event::Paste(_)
                | egui::Event::Copy
                | egui::Event::Cut
                | egui::Event::Ime(_) => true,
                _ => false,
            };
            if captured {
                pending.push(event.clone());
            }
            !captured
        });
    }
}

impl<K: Ord + Clone> TerminalSessionsOf<K> {
    pub fn get(&self, key: &K) -> Option<&TerminalSession> {
        self.sessions.get(key)
    }

    /// セッションを返します。未作成なら `cwd` で作成します (シェルは起動しません)。
    pub fn session_mut(&mut self, key: &K, cwd: &Path) -> &mut TerminalSession {
        self.sessions
            .entry(key.clone())
            .or_insert_with(|| TerminalSession::new(cwd.to_path_buf()))
    }

    /// すべてのセッションの出力を取り込みます。いずれかの画面が変化したら `true`。
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for session in self.sessions.values_mut() {
            changed |= session.poll();
        }
        changed
    }

    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }
}

impl<P: Ord + Clone> TerminalSessionsOf<Option<P>> {
    /// 削除されたプロジェクトのセッションを終了します。
    pub fn retain_projects(&mut self, exists: impl Fn(&P) -> bool) {
        self.sessions
            .retain(|key, _| key.as_ref().is_none_or(&exists));
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};

    use portable_pty::CommandBuilder;
    use workspace_ui::ProjectId;

    use super::{TerminalSessionsOf, TerminalStatus};

    type TerminalSessions = TerminalSessionsOf<Option<ProjectId>>;
    use crate::pty::{PtySession, TerminalError, TerminalSpawner, Wake};

    /// Runs a fixed script and records the cwd each session was started in.
    struct ScriptSpawner {
        script: &'static str,
        cwds: Mutex<Vec<PathBuf>>,
    }

    impl TerminalSpawner for ScriptSpawner {
        fn spawn(
            &self,
            cwd: &Path,
            rows: u16,
            cols: u16,
            wake: Wake,
        ) -> Result<PtySession, TerminalError> {
            self.cwds.lock().unwrap().push(cwd.to_path_buf());
            let mut command = CommandBuilder::new("/bin/sh");
            command.args(["-c", self.script]);
            command.cwd(cwd);
            PtySession::spawn(command, rows, cols, Some(wake))
        }
    }

    struct FailingSpawner;

    impl TerminalSpawner for FailingSpawner {
        fn spawn(&self, _: &Path, _: u16, _: u16, _: Wake) -> Result<PtySession, TerminalError> {
            Err(TerminalError::Spawn("no shell".into()))
        }
    }

    fn signal() -> (Wake, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(move || {
                let _ = tx.send(());
            }),
            rx,
        )
    }

    #[test]
    fn exited_shell_reports_status_keeps_output_and_restarts() {
        // Given: a session whose shell prints and exits with status 7
        let spawner = ScriptSpawner {
            script: "echo bye; exit 7",
            cwds: Mutex::default(),
        };
        let mut sessions = TerminalSessions::default();
        let (wake, rx) = signal();
        let cwd = std::env::temp_dir();
        sessions
            .session_mut(&None, &cwd)
            .ensure_started(&spawner, Arc::clone(&wake));

        // When: output and exit are polled until both have been observed
        while sessions.get(&None).is_none_or(|session| {
            !matches!(session.status(), TerminalStatus::Exited(_))
                || session.emulator().screen_lines()[0] != "bye"
        }) {
            rx.recv().expect("PTY must signal output or exit");
            sessions.poll();
        }

        // Then: the exit code and final output stay visible
        let session = sessions.session_mut(&None, &cwd);
        assert_eq!(session.status(), &TerminalStatus::Exited(7));
        assert_eq!(session.emulator().screen_lines()[0], "bye");

        // When: restarted
        session.restart();
        session.ensure_started(&spawner, wake);

        // Then: a fresh shell runs on a cleared screen
        assert_eq!(session.status(), &TerminalStatus::Running);
        assert_eq!(spawner.cwds.lock().unwrap().len(), 2);
    }

    #[test]
    fn spawn_failure_is_reported_without_retrying() {
        // Given: a spawner that cannot start a shell
        let mut sessions = TerminalSessions::default();
        let session = sessions.session_mut(&None, Path::new("/tmp"));
        let (wake, _rx) = signal();

        // When: the session is started twice
        session.ensure_started(&FailingSpawner, Arc::clone(&wake));
        session.ensure_started(&FailingSpawner, wake);

        // Then: the failure is kept for the pane to show
        assert_eq!(
            session.status(),
            &TerminalStatus::Failed("PTY spawn failed: no shell".into())
        );
    }

    #[test]
    fn removed_projects_drop_their_sessions() {
        // Given: sessions for two projects and the home session
        let mut sessions = TerminalSessions::default();
        let kept = Some(ProjectId::new("kept"));
        let removed = Some(ProjectId::new("removed"));
        for key in [&kept, &removed, &None] {
            sessions.session_mut(key, Path::new("/tmp"));
        }

        // When: only one project remains
        sessions.retain_projects(|project| project.to_string() == "kept");

        // Then: the removed project's session is gone and home stays
        assert!(sessions.get(&removed).is_none());
        assert!(sessions.get(&kept).is_some());
        assert!(sessions.get(&None).is_some());
    }
}
