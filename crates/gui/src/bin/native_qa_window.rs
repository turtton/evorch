//! Native GUI QA entry point with deterministic fixtures and no runtime startup.

use std::error::Error;
use std::ffi::OsString;
use std::path::PathBuf;

use gui::app::{WorkbenchApp, WorkbenchState};
use gui::fixture::{DemoSource, demo_runs, demo_sidebar, populate};
use workspace_ui::UiSettings;

const DEFAULT_TITLE: &str = "evorch native QA";

#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    window_title: String,
    layout: PathBuf,
    save_layout: PathBuf,
}

fn parse_args(mut values: impl Iterator<Item = OsString>) -> Result<Arguments, String> {
    let mut window_title = None;
    let mut layout = None;
    let mut save_layout = None;
    while let Some(argument) = values.next() {
        let option = match argument.to_str() {
            Some("--window-title" | "--layout" | "--save-layout") => {
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
            _ => unreachable!("validated option"),
        }
    }
    Ok(Arguments {
        window_title: window_title.unwrap_or_else(|| DEFAULT_TITLE.into()),
        layout: layout.ok_or("--layout is required")?,
        save_layout: save_layout.ok_or("--save-layout is required")?,
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
                _directory: directory,
            }))
        }),
    )
    .map_err(|error| format!("native window failed: {error}").into())
}

struct NativeQaApp {
    workbench: WorkbenchApp<DemoSource>,
    // Sidebar paths remain valid for the entire native event loop.
    _directory: tempfile::TempDir,
}

impl eframe::App for NativeQaApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.workbench.raw_input_hook(ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.workbench.ui(ui, frame);
    }
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        println!(
            "Usage: native_qa_window --layout PATH --save-layout PATH [--window-title TITLE]\n\n\
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
}
