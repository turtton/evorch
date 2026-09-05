use std::env;
use std::error::Error;
use std::path::PathBuf;

use gui::app::WorkbenchState;
use gui::fixture::{DemoSource, demo_runs, demo_sidebar, populate};
use gui::headless::HeadlessWorkbench;
use workspace_ui::UiSettings;

const DEFAULT_OUTPUT: &str = "target/headless-capture.png";

#[derive(Debug)]
struct CaptureArgs {
    output: PathBuf,
    demo: bool,
    activate: Option<String>,
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
    let mut activate: Option<String> = None;
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
            Some("--demo") if demo => return Err("unexpected additional arguments".into()),
            Some("--demo") => demo = true,
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
    Ok(CaptureArgs {
        output: output.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT)),
        demo,
        activate,
    })
}

fn print_help() {
    println!(
        r#"Usage: headless_capture [--demo] [--activate ID] [--out PATH] [PATH]

Captures a 1280x720 headless workbench frame as PNG.

Modes:
  (default)      empty workbench state
  --demo         deterministic populated workbench (fixture::populate)
  --activate ID  activate the given panel tab before capturing (e.g. merge-main)

The output path comes from --out PATH or a single positional PATH
(default: {DEFAULT_OUTPUT}).

Local: nix develop -c env WGPU_BACKEND=vulkan \
         cargo run -p gui --bin headless_capture -- --demo --out target/demo-smoke.png
CI:    the headless-capture job runs the same command with WGPU_BACKEND=vulkan.
"#
    );
}

#[cfg(test)]
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
}
