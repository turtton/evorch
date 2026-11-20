//! The process-level wiring of [`super::flow::run`]: configuration, terminal
//! and the Ctrl-C of this process.
use super::{
    args::Parsed,
    flow::{AdminEnv, run},
};
use crate::cli::core_client::{client_config, exit};
use core_launcher::handoff::diagnostics::{connect::ConnectTarget, reconnect::ReconnectPolicy};
use std::io::{BufRead, IsTerminal, Write};
use workbench_protocol::handshake::LocalSurface;

pub async fn run_process(parsed: Parsed) -> u8 {
    let Some(dir) = parsed.user_config.clone().or_else(config::user_config_dir) else {
        eprintln!("cannot resolve the user config directory; pass --user-config <dir>");
        return exit::FAILURE;
    };
    let target = match core_instance::ConfigIdentity::new(&dir).and_then(ConnectTarget::new) {
        Ok(target) => target,
        Err(error) => {
            eprintln!(
                "cannot use the configuration directory {}: {error}",
                dir.display()
            );
            return exit::FAILURE;
        }
    };
    let mut client = client_config(&[]);
    client.local_surface = Some(LocalSurface::Cli);
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut approve = |question: &str| {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).is_ok()
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    let mut ctrl_c = std::pin::pin!(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    run(
        parsed.command,
        AdminEnv {
            target,
            client,
            policy: ReconnectPolicy::default(),
            terminal,
            out: &mut out,
            err: &mut err,
            approve: &mut approve,
            interrupt: ctrl_c.as_mut(),
        },
    )
    .await
}
