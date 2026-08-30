use std::env;
use std::error::Error;
use std::path::PathBuf;

use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::AgentSummary;
use workspace_ui::UiSettings;

const DEFAULT_OUTPUT: &str = "target/headless-capture.png";

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let output = output_path(env::args_os().skip(1))?;
    let state = WorkbenchState::new(EmptySource, &UiSettings::default())?;
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 720.0]);
    workbench.run();
    let frame = workbench.capture()?;
    frame.save_png(&output)?;
    println!("{}", output.display());
    Ok(())
}

fn output_path(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<PathBuf, Box<dyn Error>> {
    let Some(first) = arguments.next() else {
        return Ok(PathBuf::from(DEFAULT_OUTPUT));
    };
    if first == "--out" {
        let path = arguments.next().ok_or("--out requires an output path")?;
        if arguments.next().is_some() {
            return Err("unexpected additional arguments".into());
        }
        return Ok(PathBuf::from(path));
    }
    if arguments.next().is_some() {
        return Err("expected at most one output path".into());
    }
    Ok(PathBuf::from(first))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_OUTPUT, output_path};

    #[test]
    fn output_path_defaults_to_target_capture_png() {
        // Given: no command-line arguments
        let arguments = Vec::<std::ffi::OsString>::new();

        // When: the output path is parsed
        let path = output_path(arguments.into_iter()).expect("default path must parse");

        // Then: the documented target path is selected
        assert_eq!(path, std::path::PathBuf::from(DEFAULT_OUTPUT));
    }

    #[test]
    fn output_path_accepts_positional_and_out_flag_forms() {
        // Given: each supported explicit output form
        let positional = ["custom.png"].map(std::ffi::OsString::from);
        let flagged = ["--out", "flagged.png"].map(std::ffi::OsString::from);

        // When: the output paths are parsed
        let positional_path =
            output_path(positional.into_iter()).expect("positional path must parse");
        let flagged_path = output_path(flagged.into_iter()).expect("--out path must parse");

        // Then: both forms preserve the caller's path
        assert_eq!(positional_path, std::path::PathBuf::from("custom.png"));
        assert_eq!(flagged_path, std::path::PathBuf::from("flagged.png"));
    }
}
