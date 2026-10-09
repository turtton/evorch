use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use egui::{Event, Key, Modifiers};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::pty::{PtySession, TerminalError, TerminalSpawner, Wake};
use gui::terminal::TerminalStatus;
use portable_pty::CommandBuilder;
use runtime::AgentSummary;
use workspace_ui::{PanelId, UiSettings};

#[derive(Clone)]
struct Source(Vec<AgentSummary>);

impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

/// Runs `script` under /bin/sh, recording each cwd and forwarding PTY wakeups to the test.
struct ScriptSpawner {
    script: &'static str,
    cwds: Mutex<Vec<PathBuf>>,
    wake_tx: Mutex<mpsc::Sender<()>>,
}

impl ScriptSpawner {
    fn new(script: &'static str) -> (Arc<Self>, mpsc::Receiver<()>) {
        let (wake_tx, wake_rx) = mpsc::channel();
        let spawner = Arc::new(Self {
            script,
            cwds: Mutex::default(),
            wake_tx: Mutex::new(wake_tx),
        });
        (spawner, wake_rx)
    }

    fn cwds(&self) -> Vec<PathBuf> {
        self.cwds.lock().unwrap().clone()
    }
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
        let tx = self.wake_tx.lock().unwrap().clone();
        let forward: Wake = Arc::new(move || {
            wake();
            let _ = tx.send(());
        });
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", self.script]);
        command.cwd(cwd);
        PtySession::spawn(command, rows, cols, Some(forward))
    }
}

fn terminal_harness_with(
    state: WorkbenchState<Source>,
    size: [f32; 2],
) -> HeadlessWorkbench<Source> {
    let mut harness = HeadlessWorkbench::new(state, size);
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("terminal-main"))
        .expect("terminal tab exists");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("terminal tab can be activated");
    harness
        .state_mut()
        .dock_mut()
        .leaf_mut(path.node_path())
        .expect("terminal leaf exists")
        .collapsed = false;
    harness.run();
    harness
}

fn default_state() -> WorkbenchState<Source> {
    WorkbenchState::new(Source(Vec::new()), &UiSettings::default()).expect("default state builds")
}

fn terminal_harness() -> HeadlessWorkbench<Source> {
    terminal_harness_with(default_state(), [800.0, 600.0])
}

fn screen(harness: &HeadlessWorkbench<Source>) -> Vec<String> {
    harness
        .state()
        .active_terminal()
        .expect("terminal session exists")
        .emulator()
        .screen_lines()
}

/// Steps frames on every PTY wakeup until `done` holds; the PTY signals each output and exit.
fn run_until(
    harness: &mut HeadlessWorkbench<Source>,
    wake: &mpsc::Receiver<()>,
    done: impl Fn(&HeadlessWorkbench<Source>) -> bool,
) {
    harness.step();
    while !done(harness) {
        wake.recv()
            .expect("PTY must keep signalling until the condition holds");
        harness.step();
    }
}

fn type_text(harness: &mut HeadlessWorkbench<Source>, text: &str) {
    harness.input_mut().events.push(Event::Text(text.into()));
}

fn press(harness: &mut HeadlessWorkbench<Source>, key: Key, modifiers: Modifiers) {
    for pressed in [true, false] {
        harness.input_mut().events.push(Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        });
    }
}

fn screen_contains(harness: &HeadlessWorkbench<Source>, needle: &str) -> bool {
    screen(harness).iter().any(|line| line.contains(needle))
}

// Given: output with control sequences / When: rendered / Then: the grid and its accessible text follow VT semantics.
#[test]
fn terminal_pane_renders_vt_output_as_a_grid() {
    let mut harness = terminal_harness();
    harness
        .state_mut()
        .feed_terminal(b"$ cargo test\r\n\x1b[32mok\x1b[0m 42 tests\r\nprogress 10%\rprogress 99%");
    harness.run();

    assert_eq!(
        screen(&harness)[..3],
        ["$ cargo test", "ok 42 tests", "progress 99%"]
    );
    let value = harness
        .label_value("Terminal")
        .expect("terminal exposes its text");
    assert!(value.starts_with("$ cargo test\nok 42 tests\nprogress 99%"));
}

// Given: CJK output / When: rendered / Then: double-width text stays intact.
#[test]
fn terminal_pane_renders_cjk_lines() {
    let mut harness = terminal_harness();
    harness
        .state_mut()
        .feed_terminal("コンパイル成功\r\nテスト結果: 全緑".as_bytes());
    harness.run();

    assert_eq!(
        screen(&harness)[..2],
        ["コンパイル成功", "テスト結果: 全緑"]
    );
}

// Given: no PTY or output / When: terminal tab active / Then: connection state is explicit.
#[test]
fn terminal_pane_renders_empty_buffer() {
    let harness = terminal_harness();

    assert!(harness.has_label("Projects"));
    assert!(harness.has_label("Waiting for terminal connection"));
    assert!(harness.has_label("Shell output will appear when a terminal session is connected."));
}

// Given: two window sizes / When: the pane lays out / Then: the grid follows the pane and stays on screen.
#[test]
fn terminal_grid_size_follows_the_pane() {
    let size_for = |window: [f32; 2]| {
        let harness = terminal_harness_with(default_state(), window);
        let screen_rect = harness.screen_rect();
        for rect in harness.label_rects("Terminal") {
            assert!(
                screen_rect.contains_rect(rect),
                "{rect:?} outside {screen_rect:?}"
            );
        }
        harness
            .state()
            .active_terminal()
            .expect("terminal session exists")
            .emulator()
            .size()
    };

    let small = size_for([800.0, 600.0]);
    let large = size_for([1400.0, 1000.0]);

    assert!(large.columns > small.columns, "{small:?} -> {large:?}");
    assert!(large.lines > small.lines, "{small:?} -> {large:?}");
}

// Given: a focused shell / When: the user types, including Tab / Then: keys reach the PTY and exit offers restart.
#[test]
fn focused_terminal_sends_keys_to_the_shell_and_restarts_after_exit() {
    let (spawner, wake) = ScriptSpawner::new(
        "printf 'ready\\n'; IFS= read -r line; printf 'got:[%s]\\n' \"$line\"; exit 5",
    );
    let state = default_state().with_terminal_spawner(spawner.clone());
    let mut harness = terminal_harness_with(state, [800.0, 600.0]);
    run_until(&mut harness, &wake, |h| screen_contains(h, "ready"));

    harness.click_label("Terminal");
    harness.run();
    type_text(&mut harness, "a");
    press(&mut harness, Key::Tab, Modifiers::NONE);
    type_text(&mut harness, "b");
    press(&mut harness, Key::Enter, Modifiers::NONE);
    run_until(&mut harness, &wake, |h| {
        matches!(
            h.state().active_terminal().map(|s| s.status()),
            Some(TerminalStatus::Exited(_))
        ) && screen_contains(h, "got:[")
    });

    let line = screen(&harness)
        .into_iter()
        .find(|line| line.starts_with("got:["))
        .expect("echoed line");
    assert_eq!(
        line.split_whitespace().collect::<Vec<_>>(),
        ["got:[a", "b]"]
    );
    assert_eq!(
        harness
            .state()
            .active_terminal()
            .map(|s| s.status().clone()),
        Some(TerminalStatus::Exited(5))
    );
    harness.run();
    assert!(harness.has_label("Exited with code 5 · press Enter to restart"));

    press(&mut harness, Key::Enter, Modifiers::NONE);
    run_until(&mut harness, &wake, |h| screen_contains(h, "ready"));

    assert_eq!(spawner.cwds().len(), 2);
    assert_eq!(
        harness
            .state()
            .active_terminal()
            .map(|s| s.status().clone()),
        Some(TerminalStatus::Running)
    );
}

// Given: two projects / When: the selection switches / Then: each gets its own shell in its repo root.
#[test]
fn each_project_gets_its_own_shell_in_its_root() {
    let first = tempfile::tempdir().expect("first project dir");
    let second = tempfile::tempdir().expect("second project dir");
    let (spawner, wake) = ScriptSpawner::new("pwd; exec cat");
    let mut state = default_state().with_terminal_spawner(spawner.clone());
    let first_id = state.add_project(first.path()).expect("first project");
    let second_id = state.add_project(second.path()).expect("second project");
    state
        .select_project(first_id.clone())
        .expect("select first");
    let mut harness = terminal_harness_with(state, [800.0, 600.0]);
    let first_root = first.path().display().to_string();
    run_until(&mut harness, &wake, |h| screen_contains(h, &first_root));

    harness
        .state_mut()
        .select_project(second_id)
        .expect("select second");
    let second_root = second.path().display().to_string();
    run_until(&mut harness, &wake, |h| screen_contains(h, &second_root));

    assert_eq!(spawner.cwds(), [first.path(), second.path()]);
    assert_eq!(harness.state().terminals().len(), 2);
    harness
        .state_mut()
        .select_project(first_id)
        .expect("select first again");
    harness.run();
    assert!(screen_contains(&harness, &first_root));
    assert_eq!(
        spawner.cwds().len(),
        2,
        "switching back reuses the running shell"
    );
}
