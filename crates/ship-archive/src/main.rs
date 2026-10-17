//! The server binary: parse the command line, set up logging and the runtime, and serve until
//! SIGINT or SIGTERM.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use ship_archive::cli::{Cli, Command, LogFormat, ServeArgs};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Command::Serve(args) = cli.command;
    init_logging(args.log_format);
    match serve(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: &ServeArgs) -> anyhow::Result<()> {
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(n) = args.worker_threads {
        anyhow::ensure!(n > 0, "--worker-threads must be at least 1");
        runtime.worker_threads(n);
    }
    let runtime = runtime.build()?;
    runtime.block_on(ship_archive::serve(args.config(), shutdown_signal()))
}

/// Logs go to stderr, filtered by RUST_LOG (default `info`), colored only on a terminal.
fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let logs = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Text => logs.init(),
        LogFormat::Json => logs.json().init(),
    }
}

/// Completes on the first SIGINT (Ctrl-C) or, on Unix, SIGTERM. A signal that cannot be
/// watched never completes, rather than shutting the server down at once.
async fn shutdown_signal() {
    let interrupt = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("cannot watch for Ctrl-C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(e) => {
                error!("cannot watch for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    info!("shutdown signal received");
}
