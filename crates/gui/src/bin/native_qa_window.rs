//! Native GUI QA entry point with deterministic fixtures and no runtime startup.

use std::error::Error;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gui::app::{WorkbenchApp, WorkbenchState};
use gui::fixture::{DemoSource, demo_runs, demo_sidebar, populate};
use workspace_ui::UiSettings;

const DEFAULT_TITLE: &str = "evorch native QA";

#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    window_title: String,
    layout: PathBuf,
    save_layout: PathBuf,
    minimize_after: Option<Duration>,
}

fn parse_args(mut values: impl Iterator<Item = OsString>) -> Result<Arguments, String> {
    let mut window_title = None;
    let mut layout = None;
    let mut save_layout = None;
    let mut minimize_after = None;
    while let Some(argument) = values.next() {
        let option = match argument.to_str() {
            Some("--window-title" | "--layout" | "--save-layout" | "--minimize-after-ms") => {
                argument.to_str().expect("matched a UTF-8 option")
            }
            _ => return Err(format!("unknown argument: {}", argument.to_string_lossy())),
        };
        let value = values
            .next()
            .filter(|value| !value.is_empty() && !value.to_string_lossy().starts_with("--"))
            .ok_or_else(|| format!("{option} requires a value"))?;
        match option {
            "--window-title" => {
                if window_title.is_some() {
                    return Err(format!("{option} must be specified only once"));
                }
                window_title = Some(
                    value
                        .into_string()
                        .map_err(|_| String::from("--window-title requires valid UTF-8"))?,
                );
            }
            "--layout" => {
                if layout.replace(PathBuf::from(value)).is_some() {
                    return Err(format!("{option} must be specified only once"));
                }
            }
            "--save-layout" => {
                if save_layout.replace(PathBuf::from(value)).is_some() {
                    return Err(format!("{option} must be specified only once"));
                }
            }
            "--minimize-after-ms" => {
                if minimize_after.is_some() {
                    return Err(format!("{option} must be specified only once"));
                }
                let milliseconds = value
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|milliseconds| *milliseconds > 0)
                    .ok_or("--minimize-after-ms requires a positive integer")?;
                minimize_after = Some(Duration::from_millis(milliseconds));
            }
            _ => unreachable!("validated option"),
        }
    }
    Ok(Arguments {
        window_title: window_title.unwrap_or_else(|| DEFAULT_TITLE.into()),
        layout: layout.ok_or("--layout is required")?,
        save_layout: save_layout.ok_or("--save-layout is required")?,
        minimize_after,
    })
}

fn run(arguments: Arguments) -> Result<(), Box<dyn Error>> {
    let mut settings = UiSettings::default();
    let requested_workspace = workspace_ui::load_workspace(&arguments.layout).map_err(|error| {
        format!(
            "cannot load layout '{}': {error}",
            arguments.layout.display()
        )
    })?;
    settings.layout.workspace = Some(requested_workspace.clone());
    let directory =
        tempfile::tempdir().map_err(|error| format!("cannot create fixture directory: {error}"))?;
    let sidebar = demo_sidebar(directory.path())?;
    let mut state = populate(
        WorkbenchState::new(DemoSource(demo_runs()), &settings)?,
        sidebar,
    )
    .with_save_path(arguments.save_layout);
    // Demo events add transcript tabs. Restore the requested dock arrangement so
    // the native input test starts with the exact supplied tab order.
    *state.dock_mut() = gui::dock::to_dock_state(&requested_workspace)?;

    let options = gui::window::native_options(&arguments.window_title);
    eframe::run_native(
        &arguments.window_title,
        options,
        Box::new(move |creation_context| {
            state.reload_theme(&creation_context.egui_ctx, settings.theme_preset.into());
            Ok(Box::new(NativeQaApp {
                workbench: WorkbenchApp(state),
                minimize_after: arguments.minimize_after,
                first_frame_nr: None,
                first_frame_at: None,
                _directory: directory,
            }))
        }),
    )
    .map_err(|error| format!("native window failed: {error}").into())
}

struct NativeQaApp {
    workbench: WorkbenchApp<DemoSource>,
    minimize_after: Option<Duration>,
    first_frame_nr: Option<u64>,
    first_frame_at: Option<Instant>,
    // Sidebar paths remain valid for the entire native event loop.
    _directory: tempfile::TempDir,
}

impl eframe::App for NativeQaApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.workbench.raw_input_hook(ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.workbench.ui(ui, frame);
        if let Some(delay) = self.minimize_after {
            let frame_nr = ui.ctx().cumulative_frame_nr();
            let first_frame_nr = *self.first_frame_nr.get_or_insert_with(|| {
                eprintln!("native_qa_window: first-frame-ready");
                frame_nr
            });
            if frame_nr == first_frame_nr {
                return;
            }
            // A subsequent frame means the first UI pass has returned through
            // the native renderer. Do not count multiple passes in one frame.
            let first_frame_at = self.first_frame_at.get_or_insert_with(|| {
                eprintln!("native_qa_window: redraw-ready");
                Instant::now()
            });
            if first_frame_at.elapsed() >= delay {
                // Keep the production repaint cadence. On Wayland, minimizing
                // stops compositor frame callbacks and exercises eframe's wait
                // for a pending redraw, which must not become a busy poll loop.
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                eprintln!("native_qa_window: minimize-requested");
                self.minimize_after = None;
            }
        }
    }
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        println!(
            "Usage: native_qa_window --layout PATH --save-layout PATH [--window-title TITLE] \
             [--minimize-after-ms MILLISECONDS]\n\n\
             Opens the production workbench UI with deterministic fixtures.\n\
             Ctrl+S saves the layout; closing the window exits successfully.\n\
             No model, runtime, or sandbox is started."
        );
        return;
    }
    let result = parse_args(arguments.into_iter())
        .map_err(|error| -> Box<dyn Error> { error.into() })
        .and_then(run);
    if let Err(error) = result {
        eprintln!("native_qa_window: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> Result<Arguments, String> {
        parse_args(values.iter().map(OsString::from))
    }

    #[test]
    fn accepts_explicit_title_and_layout_paths() {
        assert_eq!(
            parse(&[
                "--window-title",
                "QA window 1",
                "--layout",
                "fixtures/layout.json",
                "--save-layout",
                "artifacts/saved.json",
            ])
            .unwrap(),
            Arguments {
                window_title: "QA window 1".into(),
                layout: "fixtures/layout.json".into(),
                save_layout: "artifacts/saved.json".into(),
                minimize_after: None,
            }
        );
    }

    #[test]
    fn defaults_title_but_requires_both_layout_paths() {
        assert_eq!(parse(&[]).unwrap_err(), "--layout is required");
        assert_eq!(
            parse(&["--layout", "in.json"]).unwrap_err(),
            "--save-layout is required"
        );
        assert_eq!(
            parse(&["--layout", "in.json", "--save-layout", "out.json"])
                .unwrap()
                .window_title,
            DEFAULT_TITLE
        );
    }

    #[test]
    fn rejects_unknown_options_missing_values_and_duplicates() {
        for (values, expected) in [
            (vec!["--demo"], "unknown argument: --demo"),
            (vec!["--layout"], "--layout requires a value"),
            (
                vec!["--layout", "--save-layout", "out.json"],
                "--layout requires a value",
            ),
            (
                vec!["--layout", "in.json", "--layout", "other.json"],
                "--layout must be specified only once",
            ),
        ] {
            assert_eq!(parse(&values).unwrap_err(), expected);
        }
    }

    #[test]
    fn minimization_is_opt_in_and_requires_a_positive_duration() {
        let required = ["--layout", "in.json", "--save-layout", "out.json"];
        assert_eq!(parse(&required).unwrap().minimize_after, None);
        let mut values = required.to_vec();
        values.extend(["--minimize-after-ms", "2000"]);
        assert_eq!(
            parse(&values).unwrap().minimize_after,
            Some(Duration::from_secs(2))
        );
        for invalid in ["0", "-1", "1.5", "later", "18446744073709551616"] {
            values[5] = invalid;
            assert_eq!(
                parse(&values).unwrap_err(),
                "--minimize-after-ms requires a positive integer"
            );
        }
        values[5] = "2000";
        values.extend(["--minimize-after-ms", "3000"]);
        assert_eq!(
            parse(&values).unwrap_err(),
            "--minimize-after-ms must be specified only once"
        );
        assert_eq!(
            parse(&["--minimize-after-ms"]).unwrap_err(),
            "--minimize-after-ms requires a value"
        );
    }
}
