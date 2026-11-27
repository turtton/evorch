//! `evorch-gui`: the window of the single evorch core.
//!
//! The GUI is only ever a client of the core of a configuration directory: it
//! connects to that core, or starts the `evorch-core` installed beside this
//! executable, and it never opens the database, a runtime or an ownership
//! socket itself. The embedded runtime that existed before the single core
//! (`--legacy-embedded`, `--demo` and the options that configured it) is gone;
//! its options are refused before anything is started.
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum GuiError {
    #[error("argument error: {0}")]
    Arguments(String),
    #[error("start-up failed: {0}")]
    Startup(String),
}

#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    config_dir: Option<PathBuf>,
    window_title: String,
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Help,
    Run(Arguments),
}

/// Options of the embedded runtime that this build no longer has. They are named in
/// the error instead of being read as something else or ignored.
const REMOVED_OPTIONS: [&str; 6] = [
    "--demo",
    "--legacy-embedded",
    "--settings",
    "--layout",
    "--save-layout",
    "--state",
];

fn parse_arguments() -> Result<Request, GuiError> {
    parse_arguments_from(std::env::args().skip(1))
}

fn parse_arguments_from(mut values: impl Iterator<Item = String>) -> Result<Request, GuiError> {
    let mut arguments = Arguments {
        config_dir: None,
        window_title: String::from("evorch"),
    };
    while let Some(argument) = values.next() {
        match argument.as_str() {
            // The core client has been the only mode since 19a; the flag changes nothing.
            "--core-client" => {}
            "--config-dir" => {
                arguments.config_dir = Some(values.next().map(PathBuf::from).ok_or_else(|| {
                    GuiError::Arguments(String::from("--config-dir requires a path"))
                })?);
            }
            "--window-title" => {
                arguments.window_title = values.next().ok_or_else(|| {
                    GuiError::Arguments(String::from("--window-title requires a value"))
                })?;
            }
            "--help" | "-h" => return Ok(Request::Help),
            removed if REMOVED_OPTIONS.contains(&removed) => {
                return Err(GuiError::Arguments(format!(
                    "{removed} belonged to the embedded runtime, which no longer exists: \
                     evorch-gui is a client of the evorch core (see --help)"
                )));
            }
            unknown => return Err(GuiError::Arguments(format!("unknown option: {unknown}"))),
        }
    }
    Ok(Request::Run(arguments))
}

fn print_help() {
    println!(
        r#"Usage: evorch-gui [--config-dir PATH] [--window-title <text>]

--config-dir PATH      The configuration directory of the core to use.
--window-title <text>  Set the window title (default: evorch).

evorch-gui is a client of the single evorch core: it connects to the core of the
configuration directory, or starts the evorch-core installed beside this
executable, and it never opens the database or a runtime itself. When the core
cannot be reached or started the window says so. The configuration directory is
--config-dir, then EVORCH_CONFIG_DIR, then the user configuration directory.
--core-client is accepted and changes nothing.

Closing the window ends only the client: the core and the runs it manages keep
going.

The embedded runtime options (--demo, --legacy-embedded, --settings, --layout,
--save-layout, --state) were removed and are refused."#
    );
}

fn run(arguments: Arguments) -> Result<(), GuiError> {
    gui::logging::init();
    let directory = arguments
        .config_dir
        .or_else(|| std::env::var_os("EVORCH_CONFIG_DIR").map(PathBuf::from))
        .or_else(config::user_config_dir)
        .ok_or_else(|| GuiError::Startup("user config directory is unavailable".into()))?;
    // No fallback: a core that cannot be reached or started ends here (or is shown in
    // the window); there is no runtime in this process to use instead.
    gui::core_client::run(directory, arguments.window_title).map_err(GuiError::Startup)
}

fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("--help" | "-h"))
        && std::env::args_os().len() == 2
    {
        print_help();
        return;
    }
    // The guard bundle is bound before anything else is looked at (tests/guard_entry.rs).
    if sandbox::owned::bind_installed("gui").is_err() {
        eprintln!("evorch-gui: process guard bundle validation failed");
        std::process::exit(1);
    }
    // Then the arguments: a refused option has started nothing.
    let arguments = match parse_arguments() {
        Ok(Request::Help) => {
            print_help();
            return;
        }
        Ok(Request::Run(arguments)) => arguments,
        Err(error) => {
            eprintln!("evorch-gui: {error}");
            std::process::exit(1);
        }
    };
    let result = run(arguments);
    gui::logging::flush();
    if let Err(error) = result {
        eprintln!("evorch-gui: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{Arguments, GuiError, Request, parse_arguments_from};

    fn parse(values: &[&str]) -> Result<Request, GuiError> {
        parse_arguments_from(values.iter().map(|value| String::from(*value)))
    }

    #[test]
    fn the_client_options_are_read_and_nothing_else_is_needed() {
        assert_eq!(
            parse(&[]),
            Ok(Request::Run(Arguments {
                config_dir: None,
                window_title: "evorch".into()
            }))
        );
        assert_eq!(
            parse(&[
                "--core-client",
                "--config-dir",
                "/tmp/c",
                "--window-title",
                "t"
            ]),
            Ok(Request::Run(Arguments {
                config_dir: Some("/tmp/c".into()),
                window_title: "t".into()
            }))
        );
        assert_eq!(parse(&["--help"]), Ok(Request::Help));
        assert_eq!(parse(&["-h"]), Ok(Request::Help));
    }

    /// The options of the embedded runtime are errors, not a client run, with or
    /// without their value and whatever else is on the line.
    #[test]
    fn the_embedded_runtime_options_are_refused() {
        for values in [
            &["--demo"][..],
            &["--legacy-embedded"],
            &["--settings", "/tmp/s"],
            &["--layout", "/tmp/l"],
            &["--save-layout", "/tmp/l"],
            &["--state", "/tmp/s"],
            &["--core-client", "--demo"],
            &["--config-dir", "/tmp/c", "--legacy-embedded"],
        ] {
            let Err(GuiError::Arguments(message)) = parse(values) else {
                panic!("{values:?} must be refused");
            };
            assert!(message.contains("embedded runtime"), "{message}");
        }
        assert!(matches!(
            parse(&["--no-such-option"]),
            Err(GuiError::Arguments(message)) if message.contains("unknown option")
        ));
        assert!(parse(&["--config-dir"]).is_err());
        assert!(parse(&["--window-title"]).is_err());
    }
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod heap_tests {
    /// glibc's `struct mallinfo2` (glibc 2.33+), the 64-bit form of what CEF reads.
    #[repr(C)]
    struct MallInfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }

    unsafe extern "C" {
        fn mallinfo2() -> MallInfo2;
    }

    fn glibc_heap_bytes() -> usize {
        // SAFETY: mallinfo2 only reads allocator statistics and returns them by value.
        let info = unsafe { mallinfo2() };
        info.arena + info.hblkhd
    }

    #[test]
    fn rust_allocations_stay_out_of_the_glibc_heap_cef_measures() {
        const TOTAL: usize = 128 << 20;
        // Small blocks would land in glibc's arena, which never shrinks below its top.
        const BLOCK: usize = 16 << 10;
        let before = glibc_heap_bytes();
        let blocks: Vec<Box<[u8]>> = (0..TOTAL / BLOCK)
            .map(|index| vec![index as u8; BLOCK].into_boxed_slice())
            .collect();
        let grown = glibc_heap_bytes().saturating_sub(before);
        assert_eq!(blocks.len(), TOTAL / BLOCK);
        // Parallel tests may grow glibc a little through SQLite and other C code.
        assert!(
            grown < TOTAL / 4,
            "glibc heap grew by {grown} bytes for {TOTAL} bytes of Rust allocations"
        );
    }
}
