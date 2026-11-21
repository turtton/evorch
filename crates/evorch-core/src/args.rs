//! Entry parsing and read-only diagnostics, independent of core bootstrap.
use std::{io, path::PathBuf};

pub const USAGE: &str =
    "usage: evorch-core [--config-dir <dir>] [--web] [--foreground] [--print-renderer-helper]";
pub const HELP: &str = concat!(
    "usage: evorch-core [--config-dir <dir>] [--web] [--foreground] [--print-renderer-helper]\n",
    "\n  --print-renderer-helper  read-only diagnostic: print the configured helper path;\n",
    "                           no config, DB, socket or lock; guard bundle verification applies.\n",
    "                           EVORCH_RENDERER_EXEC overrides the colocated default.\n",
);

#[derive(Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Serve,
    Help,
    PrintRendererHelper,
}
#[derive(Debug, Default)]
pub struct Args {
    pub config_dir: Option<PathBuf>,
    pub web: bool,
    pub launch_fd: Option<i32>,
    pub mode: Mode,
}
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config-dir" => parsed.config_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            "--web" => parsed.web = true,
            "--foreground" => (),
            "--launch-fd" if parsed.launch_fd.is_none() => {
                parsed.launch_fd = Some(
                    args.next()
                        .ok_or(USAGE)?
                        .parse::<i32>()
                        .map_err(|error| error.to_string())?,
                );
            }
            "--print-renderer-helper" if parsed.mode == Mode::Serve => {
                parsed.mode = Mode::PrintRendererHelper;
            }
            "--help" | "-h" => {
                parsed.mode = Mode::Help;
                return Ok(parsed);
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

/// Returns true when entry is complete. The normal serve branch does not call
/// the resolver; neither diagnostic enters configuration/ownership bootstrap.
pub fn write_read_only(
    mode: &Mode,
    output: &mut impl io::Write,
    resolve: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<bool> {
    match mode {
        Mode::Serve => return Ok(false),
        Mode::Help => write!(output, "{HELP}")?,
        Mode::PrintRendererHelper => {
            let path = resolve()?;
            // Preserve OS path bytes, including non-UTF-8 Unix overrides.
            output.write_all(path.as_os_str().as_encoded_bytes())?;
            output.write_all(b"\n")?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse_args(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }
    #[test]
    fn parses_diagnostic_and_writes_only_the_resolved_path() {
        let args = parse_args(&[
            "--config-dir",
            "/must/not/be/opened",
            "--launch-fd",
            "999",
            "--print-renderer-helper",
        ])
        .unwrap();
        assert_eq!(args.mode, Mode::PrintRendererHelper);
        let mut output = Vec::new();
        assert!(
            write_read_only(&args.mode, &mut output, || {
                Ok(PathBuf::from("/installed/libexec/helper with spaces"))
            })
            .unwrap()
        );
        assert_eq!(output, b"/installed/libexec/helper with spaces\n");
    }
    #[test]
    fn invalid_diagnostic_arguments_and_resolution_fail_without_output() {
        for args in [
            vec!["--print-renderer-helper", "--unknown"],
            vec!["--print-renderer-helper", "--print-renderer-helper"],
            vec!["--config-dir"],
        ] {
            assert!(parse_args(&args).is_err());
        }
        let mut output = Vec::new();
        assert!(
            write_read_only(&Mode::PrintRendererHelper, &mut output, || {
                Err(io::Error::other("cannot resolve helper"))
            })
            .is_err()
        );
        assert!(output.is_empty());
    }
    #[test]
    fn help_and_serve_do_not_resolve_or_start_anything() {
        let mut output = Vec::new();
        assert!(
            write_read_only(
                &parse_args(&["--help"]).unwrap().mode,
                &mut output,
                || panic!("help must not resolve"),
            )
            .unwrap()
        );
        assert!(String::from_utf8(output).unwrap().contains("read-only"));
        assert!(
            !write_read_only(&Mode::Serve, &mut Vec::new(), || panic!(
                "normal entry resolves only when composing the owner"
            ))
            .unwrap()
        );
    }
}
