use std::env;
use std::error::Error;
use std::path::PathBuf;

use gui::app::WorkbenchState;
use gui::fixture::{DemoSource, demo_error_events, demo_runs, demo_sidebar, populate};
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ProviderStatus;
use workspace_ui::UiSettings;

const DEFAULT_OUTPUT: &str = "target/headless-capture.png";

#[derive(Debug)]
struct CaptureArgs {
    output: PathBuf,
    demo: bool,
    error_thread: bool,
    provider_configured: bool,
    open_settings: bool,
    activate: Option<String>,
    pointer: Option<(f32, f32)>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<std::ffi::OsString> = env::args_os().skip(1).collect();
    if arguments.iter().any(|argument| {
        let text = argument.to_string_lossy();
        text == "--help" || text == "-h"
    }) {
        print_help();
        return Ok(());
    }
    let capture = parse_args(arguments.into_iter())?;
    let demo_dir = capture.demo.then(tempfile::tempdir).transpose()?;
    let mut state = match demo_dir.as_ref() {
        Some(dir) => {
            let sidebar = demo_sidebar(dir.path())?;
            populate(
                WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())?,
                sidebar,
            )
        }
        None => WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())?,
    };
    if capture.provider_configured {
        state = state.with_provider_status(ProviderStatus::Configured);
    }
    if capture.open_settings {
        let settings = state.provider_settings_mut();
        settings.name = "local".into();
        settings.base_url = "https://api.example.invalid/v1".into();
        settings.api_key_env = "EXAMPLE_API_KEY".into();
        settings.models_text = "gpt-4.1\ngpt-4.1-mini".into();
        settings.default_model = "gpt-4.1".into();
        settings.open = true;
    }
    if let Some(dir) = demo_dir.as_ref() {
        state = state.with_provider_settings_path(dir.path().join("evorch.toml"));
    }
    if capture.error_thread {
        state.apply_events(demo_error_events());
    }
    if let Some(id) = capture.activate.as_deref() {
        let path = state
            .dock()
            .find_tab(&workspace_ui::PanelId::new(id))
            .ok_or_else(|| format!("--activate: no tab for panel id '{id}'"))?;
        state
            .dock_mut()
            .set_active_tab(path)
            .map_err(|_| format!("--activate: failed to activate '{id}'"))?;
    }
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 720.0]);
    workbench.run();
    if let Some((x, y)) = capture.pointer {
        workbench.pointer_move(egui::pos2(x, y));
        // hover スタイルを反映させるため再度安定化する
        workbench.run();
    }
    let frame = workbench.capture()?;
    frame.save_png(&capture.output)?;
    println!("{}", capture.output.display());
    Ok(())
}

fn parse_args(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<CaptureArgs, Box<dyn Error>> {
    let mut output: Option<PathBuf> = None;
    let mut demo = false;
    let mut error_thread = false;
    let mut provider_configured = false;
    let mut open_settings = false;
    let mut activate: Option<String> = None;
    let mut pointer: Option<(f32, f32)> = None;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--out") => {
                let path = arguments.next().ok_or("--out requires an output path")?;
                output = Some(PathBuf::from(path));
            }
            Some("--activate") => {
                if activate.is_some() {
                    return Err("unexpected additional arguments".into());
                }
                let id = arguments.next().ok_or("--activate requires a panel id")?;
                activate = Some(id.to_string_lossy().into_owned());
            }
            Some("--pointer") => {
                if pointer.is_some() {
                    return Err("unexpected additional arguments".into());
                }
                let x = arguments
                    .next()
                    .ok_or("--pointer requires X and Y coordinates")?;
                let y = arguments
                    .next()
                    .ok_or("--pointer requires X and Y coordinates")?;
                let x: f32 = x
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "--pointer requires numeric coordinates")?;
                let y: f32 = y
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "--pointer requires numeric coordinates")?;
                pointer = Some((x, y));
            }
            Some("--demo") if demo => return Err("unexpected additional arguments".into()),
            Some("--demo") => demo = true,
            Some("--error-thread") if error_thread => {
                return Err("unexpected additional arguments".into());
            }
            Some("--error-thread") => error_thread = true,
            Some("--provider-configured") if provider_configured => {
                return Err("unexpected additional arguments".into());
            }
            Some("--provider-configured") => provider_configured = true,
            Some("--open-settings") if open_settings => {
                return Err("unexpected additional arguments".into());
            }
            Some("--open-settings") => open_settings = true,
            Some(flag) if flag.starts_with('-') => {
                return Err("unexpected additional arguments".into());
            }
            _ => {
                if output.is_some() {
                    return Err("expected at most one output path".into());
                }
                output = Some(PathBuf::from(argument));
            }
        }
    }
    if error_thread && !demo {
        return Err("--error-thread requires --demo".into());
    }
    Ok(CaptureArgs {
        output: output.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT)),
        demo,
        error_thread,
        provider_configured,
        open_settings,
        activate,
        pointer,
    })
}

fn print_help() {
    println!(
        r#"Usage: headless_capture [--demo] [--error-thread] [--provider-configured] [--open-settings] [--activate ID] [--pointer X Y] [--out PATH] [PATH]

Captures a 1280x720 headless workbench frame as PNG.

Modes:
   (default)      empty workbench state
   --demo         deterministic populated workbench (fixture::populate)
   --error-thread  with --demo: mark the active demo thread as Error (red status dot)
   --provider-configured  enable the composer without provider setup guidance (capture only)
   --open-settings  open the provider settings modal with demo values pre-filled
   --activate ID  activate the given panel tab before capturing (e.g. diff-main)
  --pointer X Y  move the pointer to (X, Y) before capturing (hover-state captures)

The output path comes from --out PATH or a single positional PATH
(default: {DEFAULT_OUTPUT}).

Local: nix develop -c env WGPU_BACKEND=vulkan \
         cargo run -p gui --bin headless_capture -- --demo --out target/demo-smoke.png
CI:    the headless-capture job runs the same command with WGPU_BACKEND=vulkan.
"#
    );
}

#[cfg(test)]
// allow: SIZE_OK — private CLI parser の既存BDDテストを同一ファイルに維持するため。
mod tests {
    use super::{DEFAULT_OUTPUT, parse_args};

    fn args<const N: usize>(values: [&str; N]) -> impl Iterator<Item = std::ffi::OsString> {
        values.map(std::ffi::OsString::from).into_iter()
    }

    #[test]
    fn parse_args_defaults_to_target_capture_png() {
        // Given: no command-line arguments
        let arguments = Vec::<std::ffi::OsString>::new();

        // When: the arguments are parsed
        let capture = parse_args(arguments.into_iter()).expect("default path must parse");

        // Then: the documented target path is selected without demo mode
        assert_eq!(capture.output, std::path::PathBuf::from(DEFAULT_OUTPUT));
        assert!(!capture.demo);
        assert!(!capture.provider_configured);
        assert!(!capture.open_settings);
    }

    #[test]
    fn parse_args_accepts_open_settings_flag() {
        // Given: settings requested alongside demo mode
        let arguments = args(["--demo", "--open-settings"]);
        // When: the arguments are parsed
        let capture = parse_args(arguments).expect("open-settings demo form must parse");
        // Then: the settings modal is requested
        assert!(capture.open_settings);
    }

    #[test]
    fn parse_args_accepts_open_settings_without_demo() {
        // Given: settings requested without demo mode
        let arguments = args(["--open-settings"]);
        // When: the arguments are parsed
        let capture = parse_args(arguments).expect("standalone open-settings must parse");
        // Then: no demo requirement is imposed
        assert!(capture.open_settings);
        assert!(!capture.demo);
    }

    #[test]
    fn parse_args_rejects_duplicate_open_settings() {
        // Given: settings requested twice
        let arguments = args(["--open-settings", "--open-settings"]);
        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("duplicate --open-settings must fail");
        // Then: the existing unexpected-arguments error is reported
        assert_eq!(error.to_string(), "unexpected additional arguments");
    }

    #[test]
    fn parse_args_accepts_positional_and_out_flag_forms() {
        // Given: each supported explicit output form
        let positional = args(["custom.png"]);
        let flagged = args(["--out", "flagged.png"]);

        // When: the arguments are parsed
        let positional_capture = parse_args(positional).expect("positional path must parse");
        let flagged_capture = parse_args(flagged).expect("--out path must parse");

        // Then: both forms preserve the caller's path
        assert_eq!(
            positional_capture.output,
            std::path::PathBuf::from("custom.png")
        );
        assert_eq!(
            flagged_capture.output,
            std::path::PathBuf::from("flagged.png")
        );
    }

    #[test]
    fn parse_args_accepts_demo_flag_in_any_position() {
        // Given: --demo alone, before --out, and after a positional path
        let forms = [
            vec!["--demo"],
            vec!["--demo", "--out", "x.png"],
            vec!["x.png", "--demo"],
        ];

        // When: each form is parsed
        let parsed = forms.map(|form| {
            parse_args(form.into_iter().map(std::ffi::OsString::from))
                .expect("demo form must parse")
        });

        // Then: demo mode is on and the output paths are preserved
        assert!(parsed.iter().all(|capture| capture.demo));
        assert_eq!(parsed[1].output, std::path::PathBuf::from("x.png"));
        assert_eq!(parsed[2].output, std::path::PathBuf::from("x.png"));
    }

    #[test]
    fn parse_args_accepts_provider_configured_with_demo() {
        // Given: an explicitly configured provider in demo mode
        let arguments = args(["--demo", "--provider-configured"]);

        // When: the arguments are parsed
        let capture = parse_args(arguments).expect("configured-provider demo form must parse");

        // Then: the capture mode is accepted
        assert!(capture.provider_configured);
    }

    #[test]
    fn parse_args_accepts_error_thread_with_demo() {
        // Given: --error-thread alongside --demo
        let arguments = args(["--demo", "--error-thread"]);

        // When: the arguments are parsed
        let capture = parse_args(arguments).expect("error-thread demo form must parse");

        // Then: the demo error-thread mode is enabled
        assert!(capture.error_thread);
    }

    #[test]
    fn parse_args_rejects_error_thread_without_demo() {
        // Given: --error-thread without --demo
        let arguments = args(["--error-thread"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("error-thread without demo must fail");

        // Then: the demo-mode requirement is reported
        assert_eq!(error.to_string(), "--error-thread requires --demo");
    }

    #[test]
    fn parse_args_rejects_duplicate_error_thread() {
        // Given: --error-thread supplied twice in demo mode
        let arguments = args(["--demo", "--error-thread", "--error-thread"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("duplicate --error-thread must fail");

        // Then: the existing unexpected-arguments error is reported
        assert_eq!(error.to_string(), "unexpected additional arguments");
    }

    #[test]
    fn parse_args_rejects_duplicate_demo() {
        // Given: --demo supplied twice
        let arguments = args(["--demo", "--demo"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("duplicate --demo must fail");

        // Then: the existing unexpected-arguments error is reported
        assert_eq!(error.to_string(), "unexpected additional arguments");
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        // Given: an unrecognized flag
        let arguments = args(["--wat"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("unknown flag must fail");

        // Then: the existing unexpected-arguments error is reported
        assert_eq!(error.to_string(), "unexpected additional arguments");
    }

    #[test]
    fn parse_args_accepts_activate_flag() {
        // Given: --activate with a panel id alongside other flags
        let arguments = args(["--demo", "--activate", "merge-main", "--out", "x.png"]);

        // When: the arguments are parsed
        let capture = parse_args(arguments).expect("activate form must parse");

        // Then: the panel id is preserved and other flags still apply
        assert_eq!(capture.activate.as_deref(), Some("merge-main"));
        assert!(capture.demo);
        assert_eq!(capture.output, std::path::PathBuf::from("x.png"));
    }

    #[test]
    fn parse_args_rejects_activate_without_value() {
        // Given: --activate without a following panel id
        let arguments = args(["--activate"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("missing --activate value must fail");

        // Then: the missing-value error is reported
        assert_eq!(error.to_string(), "--activate requires a panel id");
    }

    #[test]
    fn parse_args_rejects_duplicate_activate() {
        // Given: --activate supplied twice
        let arguments = args(["--activate", "merge-main", "--activate", "goal-main"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("duplicate --activate must fail");

        // Then: the existing unexpected-arguments error is reported
        assert_eq!(error.to_string(), "unexpected additional arguments");
    }

    #[test]
    fn parse_args_rejects_out_without_path() {
        // Given: --out without a following path
        let arguments = args(["--out"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("missing --out value must fail");

        // Then: the existing missing-value error is reported
        assert_eq!(error.to_string(), "--out requires an output path");
    }

    #[test]
    fn parse_args_accepts_pointer_coordinates() {
        // Given: --pointer with numeric X and Y alongside other flags
        let arguments = args(["--demo", "--pointer", "600.5", "37", "--out", "x.png"]);

        // When: the arguments are parsed
        let capture = parse_args(arguments).expect("pointer form must parse");

        // Then: the coordinates are preserved and other flags still apply
        assert_eq!(capture.pointer, Some((600.5, 37.0)));
        assert!(capture.demo);
    }

    #[test]
    fn parse_args_rejects_pointer_without_coordinates() {
        // Given: --pointer without both coordinates
        let missing = args(["--pointer"]);
        let partial = args(["--pointer", "10"]);

        // When: the arguments are parsed
        let missing_error = parse_args(missing).expect_err("missing coordinates must fail");
        let partial_error = parse_args(partial).expect_err("partial coordinates must fail");

        // Then: the coordinate error is reported
        assert_eq!(
            missing_error.to_string(),
            "--pointer requires X and Y coordinates"
        );
        assert_eq!(
            partial_error.to_string(),
            "--pointer requires X and Y coordinates"
        );
    }

    #[test]
    fn parse_args_rejects_non_numeric_pointer() {
        // Given: --pointer with a non-numeric coordinate
        let arguments = args(["--pointer", "left", "37"]);

        // When: the arguments are parsed
        let error = parse_args(arguments).expect_err("non-numeric coordinates must fail");

        // Then: the numeric error is reported
        assert_eq!(error.to_string(), "--pointer requires numeric coordinates");
    }
}
