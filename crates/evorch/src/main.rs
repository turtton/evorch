//! evorch コマンドのバイナリエントリポイントです。コマンドの振り分けは `evorch::cli` です。

use std::process::ExitCode;

fn main() -> ExitCode {
    // The sandboxed run_js worker (ADR 0039) must not initialise logging or panic capture.
    if std::env::args().nth(1).as_deref() == Some("js-worker") {
        return evorch::js_worker::main();
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    // Panicking runs then report where they panicked, not only the payload.
    runtime::panic_capture::install();

    evorch::cli::dispatch(std::env::args().skip(1).collect())
}
