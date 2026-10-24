use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

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

/// Steps frames on every PTY wakeup until `done` holds, with a total deadline.
#[track_caller]
fn run_until(
    harness: &mut HeadlessWorkbench<Source>,
    wake: &mpsc::Receiver<()>,
    done: impl Fn(&HeadlessWorkbench<Source>) -> bool,
) {
    // Leave room for shared CI load, but report the screen before nextest's 180s cutoff.
    run_until_before(
        harness,
        wake,
        Instant::now() + Duration::from_secs(60),
        done,
    );
}

#[track_caller]
fn run_until_before(
    harness: &mut HeadlessWorkbench<Source>,
    wake: &mpsc::Receiver<()>,
    deadline: Instant,
    done: impl Fn(&HeadlessWorkbench<Source>) -> bool,
) {
    harness.step();
    while !done(harness) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = if remaining.is_zero() {
            Err(mpsc::RecvTimeoutError::Timeout)
        } else {
            wake.recv_timeout(remaining)
        };
        if let Err(error) = result {
            let terminal = harness.state().active_terminal().expect("active terminal");
            panic!(
                "PTY condition not met: {error}; status={:?}, size={:?}, screen={:#?}",
                terminal.status(),
                terminal.emulator().size(),
                screen(harness)
            );
        }
        harness.step();
    }
}

#[test]
#[should_panic(expected = "PTY condition not met: timed out waiting on channel; status=")]
fn missing_pty_wakeup_has_a_bounded_wait() {
    let mut harness = terminal_harness();
    let (_tx, wake) = mpsc::channel();
    run_until_before(
        &mut harness,
        &wake,
        Instant::now() + Duration::from_millis(100),
        |_| false,
    );
}

#[test]
#[should_panic(expected = "PTY condition not met: timed out waiting on channel; status=")]
fn queued_pty_wakeups_do_not_extend_the_deadline() {
    let mut harness = terminal_harness();
    let (tx, wake) = mpsc::channel();
    tx.send(()).expect("queued wakeup");
    let checks = std::cell::Cell::new(0);
    run_until_before(&mut harness, &wake, Instant::now(), |_| {
        assert_eq!(checks.replace(checks.get() + 1), 0, "deadline was extended");
        false
    });
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
    // The exit can be observed before the reader thread delivers the line's tail.
    run_until(&mut harness, &wake, |h| {
        matches!(
            h.state().active_terminal().map(|s| s.status()),
            Some(TerminalStatus::Exited(_))
        ) && screen_contains(h, "b]")
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
    let projects = tempfile::tempdir().expect("project directories");
    // Force pwd to wrap at the pane's width even when TMPDIR is short (as in CI).
    let parent = projects.path().join("long-project-root-".repeat(6));
    let first = parent.join("first");
    let second = parent.join("second");
    std::fs::create_dir_all(&first).expect("first project dir");
    std::fs::create_dir_all(&second).expect("second project dir");
    // Check the full cwd outside the screen: wrapping/scrollback can hide parts of it.
    let (spawner, wake) = ScriptSpawner::new(
        "pwd > shell-cwd; cat shell-cwd; printf 'ready:%s\\n' \"${PWD##*/}\"; exec cat",
    );
    let mut state = default_state().with_terminal_spawner(spawner.clone());
    let first_id = state.add_project(&first).expect("first project");
    let second_id = state.add_project(&second).expect("second project");
    state
        .select_project(first_id.clone())
        .expect("select first");
    let mut harness = terminal_harness_with(state, [800.0, 600.0]);
    run_until(&mut harness, &wake, |h| screen_contains(h, "ready:first"));

    harness
        .state_mut()
        .select_project(second_id)
        .expect("select second");
    run_until(&mut harness, &wake, |h| screen_contains(h, "ready:second"));

    for project in [&first, &second] {
        assert_eq!(
            std::fs::read_to_string(project.join("shell-cwd")).expect("shell records its cwd"),
            format!(
                "{}\n",
                project.canonicalize().expect("project root").display()
            )
        );
    }
    assert_eq!(spawner.cwds(), [first, second]);
    assert_eq!(harness.state().terminals().len(), 2);
    harness
        .state_mut()
        .select_project(first_id)
        .expect("select first again");
    harness.run();
    assert!(screen_contains(&harness, "ready:first"));
    assert_eq!(
        spawner.cwds().len(),
        2,
        "switching back reuses the running shell"
    );
}
