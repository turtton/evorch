//! `evorch-renderer`: the out-of-process browser engine host of the evorch
//! core (ADR 0045, plan 16a).
//!
//! Started by the core with its control channel on stdin and its frame channel
//! on stdout (see `renderer_ipc::stdio_channels`). It owns CEF so the core
//! never links it, serves requests until the core says `Shutdown` or closes
//! the control channel, and exits. It does not supervise, restart or outlive
//! the core. CEF's own child processes are started from the separate
//! `evorch-cef-helper` binary, which must sit next to this one (or be named by
//! `EVORCH_CEF_HELPER`).
//!
//! ```text
//! evorch-renderer --cache-dir <absolute dir> [--log-file <file>]
//! ```
//!
//! `--cache-dir` is the CEF profile root; give every helper its own. Exit code
//! 0 after a requested shutdown or a closed control channel, 1 for a protocol
//! or channel failure, 2 when it could not start.

mod backend;
mod stdio;

use std::path::PathBuf;
use std::process::ExitCode;

use browser_cef::RuntimeConfig;
use renderer_ipc::{ExitReason, ServeConfig, serve};

struct Args {
    cache_dir: PathBuf,
    log_file: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut cache_dir = None;
    let mut log_file = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.to_str() {
            Some("--cache-dir") => cache_dir = Some(value("--cache-dir")?),
            Some("--log-file") => log_file = Some(value("--log-file")?),
            _ => return Err(format!("unknown argument {flag:?}")),
        }
    }
    let cache_dir = cache_dir.ok_or("--cache-dir is required")?;
    if !cache_dir.is_absolute() {
        return Err("--cache-dir must be an absolute path".into());
    }
    Ok(Args {
        cache_dir,
        log_file,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("evorch-renderer: {error}");
            return ExitCode::from(2);
        }
    };
    let channels = match stdio::take_channels() {
        Ok(channels) => channels,
        Err(error) => {
            eprintln!("evorch-renderer: cannot take the IPC channels: {error}");
            return ExitCode::from(2);
        }
    };
    // The core's private generation profiles carry a lease. Holding it here
    // prevents next-core cleanup from deleting a still-running helper's data.
    // Standalone callers without an owner.lock keep their existing behavior.
    let lease = args.cache_dir.join("owner.lock");
    let _profile_lease = if lease.exists() {
        match std::fs::File::open(&lease).and_then(|file| {
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
            Ok(file)
        }) {
            Ok(file) => Some(file),
            Err(error) => {
                eprintln!("evorch-renderer: profile lease: {error}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    let config = RuntimeConfig {
        root_cache_path: args.cache_dir,
        log_file: args.log_file,
        ..RuntimeConfig::offline()
    };
    let reason = serve(
        channels.control,
        channels.frames,
        ServeConfig::default(),
        |ctx| backend::CefBackend::start(ctx, config),
    );
    eprintln!("evorch-renderer: stopped: {reason:?}");
    match reason {
        ExitReason::ShutdownRequested | ExitReason::PeerClosed => ExitCode::SUCCESS,
        ExitReason::InitFailed(_) | ExitReason::HandshakeFailed(_) => ExitCode::from(2),
        ExitReason::ProtocolViolation(_) | ExitReason::FrameChannelFailed(_) => ExitCode::from(1),
    }
}
