use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use cef::{args::Args, *};

use crate::offline::{DISABLE_WEBRTC_SWITCH, WEBRTC_GLOBALS, browser_switches};

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const UI_CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub const HELPER_NAME: &str = "evorch-cef-helper";

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub helper_path: PathBuf,
    pub root_cache_path: PathBuf,
    pub switches: Vec<String>,
    /// Disables the Chromium sandbox. The SUID `chrome-sandbox` is never used,
    /// so this is only for hosts that forbid unprivileged user namespaces.
    pub no_sandbox: bool,
    pub log_file: Option<PathBuf>,
}

impl RuntimeConfig {
    /// Offline settings with the helper next to the current executable.
    /// `EVORCH_CEF_HELPER`, `EVORCH_CEF_NO_SANDBOX` and `EVORCH_CEF_LOG` override them.
    pub fn offline() -> Self {
        let helper_path = std::env::var_os("EVORCH_CEF_HELPER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap_or_default()
                    .with_file_name(HELPER_NAME)
            });
        Self {
            helper_path,
            root_cache_path: std::env::temp_dir()
                .join(format!("evorch-cef-{}", std::process::id())),
            switches: browser_switches(),
            no_sandbox: std::env::var_os("EVORCH_CEF_NO_SANDBOX").is_some(),
            log_file: std::env::var_os("EVORCH_CEF_LOG").map(PathBuf::from),
        }
    }
}

#[derive(Default)]
struct ContextReady {
    ready: Mutex<bool>,
    changed: Condvar,
}

/// The process-wide CEF instance. CEF runs its own UI thread
/// (`multi_threaded_message_loop`), so eframe keeps the winit loop. Create and
/// drop it on the same thread, after every browser has closed; CEF cannot be
/// initialized again in the same process.
pub struct CefRuntime {
    config: RuntimeConfig,
    shut_down: bool,
}

impl CefRuntime {
    pub fn initialize(config: RuntimeConfig) -> Result<Self, String> {
        if !config.helper_path.is_file() {
            return Err(format!(
                "CEF helper not found at {}",
                config.helper_path.display()
            ));
        }
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = Args::new();
        let ready = Arc::new(ContextReady::default());
        let mut app = PreviewApp::new(Arc::new(config.switches.clone()), Arc::clone(&ready));
        std::fs::create_dir_all(&config.root_cache_path).map_err(|error| error.to_string())?;
        let path = |path: &std::path::Path| CefString::from(path.to_string_lossy().as_ref());
        let settings = Settings {
            windowless_rendering_enabled: 1,
            multi_threaded_message_loop: 1,
            no_sandbox: config.no_sandbox.into(),
            browser_subprocess_path: path(&config.helper_path),
            root_cache_path: path(&config.root_cache_path),
            log_file: config.log_file.as_deref().map(path).unwrap_or_default(),
            log_severity: if config.log_file.is_some() {
                LogSeverity::INFO
            } else {
                LogSeverity::WARNING
            },
            persist_session_cookies: 0,
            ..Default::default()
        };
        if initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ) != 1
        {
            return Err(format!(
                "CEF initialization failed (exit code {})",
                get_exit_code()
            ));
        }
        let guard = ready.ready.lock().unwrap_or_else(|e| e.into_inner());
        let (_guard, wait) = ready
            .changed
            .wait_timeout_while(guard, INIT_TIMEOUT, |ready| !*ready)
            .unwrap_or_else(|e| e.into_inner());
        if wait.timed_out() {
            return Err("CEF context was not initialized in time".into());
        }
        Ok(Self {
            config,
            shut_down: false,
        })
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
}

impl Drop for CefRuntime {
    fn drop(&mut self) {
        if !self.shut_down {
            self.shut_down = true;
            cef::shutdown();
            let _ = std::fs::remove_dir_all(&self.config.root_cache_path);
        }
    }
}

/// The `App` for CEF subprocesses (`evorch-cef-helper`).
pub fn subprocess_app() -> App {
    PreviewApp::new(Arc::new(Vec::new()), Arc::new(ContextReady::default()))
}

wrap_app! {
    struct PreviewApp {
        switches: Arc<Vec<String>>,
        ready: Arc<ContextReady>,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let is_browser = process_type.is_none_or(|kind| kind.to_string().is_empty());
            let Some(command_line) = command_line else { return };
            if !is_browser {
                return;
            }
            for switch in self.switches.iter() {
                match switch.split_once('=') {
                    Some((name, value)) => command_line.append_switch_with_value(
                        Some(&CefString::from(name)),
                        Some(&CefString::from(value)),
                    ),
                    None => command_line.append_switch(Some(&CefString::from(switch.as_str()))),
                }
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(PreviewBrowserProcess::new(Arc::clone(&self.ready), Arc::clone(&self.switches)))
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            let disable = command_line_get_global().is_some_and(|line| {
                line.has_switch(Some(&CefString::from(DISABLE_WEBRTC_SWITCH))) == 1
            });
            disable.then(WebRtcBlocker::new)
        }
    }
}

wrap_browser_process_handler! {
    struct PreviewBrowserProcess {
        ready: Arc<ContextReady>,
        switches: Arc<Vec<String>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            *self.ready.ready.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.ready.changed.notify_all();
        }

        fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
            let Some(command_line) = command_line else { return };
            if self.switches.iter().any(|switch| switch == DISABLE_WEBRTC_SWITCH) {
                command_line.append_switch(Some(&CefString::from(DISABLE_WEBRTC_SWITCH)));
            }
        }
    }
}

wrap_render_process_handler! {
    struct WebRtcBlocker;

    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            if let Some(global) = context.and_then(|context| context.global()) {
                for name in WEBRTC_GLOBALS {
                    global.delete_value_bykey(Some(&CefString::from(*name)));
                }
            }
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

wrap_task! {
    struct UiTask {
        job: Arc<Mutex<Option<Job>>>,
    }

    impl Task {
        fn execute(&self) {
            if let Some(job) = self.job.lock().unwrap_or_else(|e| e.into_inner()).take() {
                job();
            }
        }
    }
}

/// Moves CEF handles across threads. CEF reference counting is atomic, and
/// every call through these handles is thread-safe by CEF's contract or runs on
/// the CEF UI thread through [`on_ui`].
pub(crate) struct SendCell<T>(pub T);
// SAFETY: see the type documentation.
unsafe impl<T> Send for SendCell<T> {}
// SAFETY: see the type documentation.
unsafe impl<T> Sync for SendCell<T> {}

impl<T: Clone> Clone for SendCell<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// Runs `job` on the CEF UI thread and waits for its result.
pub(crate) fn on_ui<R: Send + 'static>(job: impl FnOnce() -> R + Send + 'static) -> Option<R> {
    if currently_on(ThreadId::UI) == 1 {
        return Some(job());
    }
    let (sender, receiver) = mpsc::channel();
    if !post_ui(move || {
        let _ = sender.send(job());
    }) {
        return None;
    }
    receiver.recv_timeout(UI_CALL_TIMEOUT).ok()
}

/// Queues `job` on the CEF UI thread without waiting; false if CEF refused it.
pub(crate) fn post_ui(job: impl FnOnce() + Send + 'static) -> bool {
    if currently_on(ThreadId::UI) == 1 {
        job();
        return true;
    }
    let job: Job = Box::new(job);
    let mut task = UiTask::new(Arc::new(Mutex::new(Some(job))));
    post_task(ThreadId::UI, Some(&mut task)) == 1
}
