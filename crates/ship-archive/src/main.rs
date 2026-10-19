//! The server binary: parse the command line, set up logging, and run one command: `serve`
//! until SIGINT or SIGTERM, or one of the offline commands `check`, `index` and `abi`.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use clap::Parser;
use ship_archive::check::{self, Beside};
use ship_archive::cli::{AbiArgs, CheckArgs, Cli, Command, IndexArgs, LogFormat, ServeArgs};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = match &cli.command {
        Command::Serve(args) => args.log_format,
        _ => LogFormat::Text,
    };
    init_logging(format);
    let done = match cli.command {
        Command::Serve(args) => serve(&args).map(|()| ExitCode::SUCCESS),
        Command::Check(args) => check(&args),
        Command::Index(args) => index(&args).map(|()| ExitCode::SUCCESS),
        Command::Abi(args) => abi(&args).map(|()| ExitCode::SUCCESS),
    };
    done.unwrap_or_else(|e| {
        error!("{e:#}");
        ExitCode::FAILURE
    })
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

/// The report goes to standard output, as text or JSON; the exit code says how it went.
fn check(args: &CheckArgs) -> anyhow::Result<ExitCode> {
    let report = check::check(&args.config());
    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(out, "{:#}", report.json())?;
    } else {
        write!(out, "{}", report.text())?;
    }
    out.flush()?;
    Ok(ExitCode::from(report.exit_code(args.strict)))
}

/// One line per log on standard output, then what was written.
fn index(args: &IndexArgs) -> anyhow::Result<()> {
    let done = check::index(&args.config())?;
    if done.is_empty() {
        warn!("no state-history log found");
    }
    let mut out = std::io::stdout().lock();
    for indexed in &done {
        writeln!(out, "{}", indexed.line())?;
    }
    let written = done.iter().filter(|i| i.written.is_some()).count();
    writeln!(
        out,
        "{written} of {} indexes written to {}",
        done.len(),
        args.out.display()
    )?;
    out.flush()?;
    if done.iter().any(|i| i.beside == Beside::Wrong) {
        warn!(
            "some logs have a wrong index beside them, left as it is: serve them with \
             --index-dir {}",
            args.out.display()
        );
    }
    Ok(())
}

/// The ABI text goes to `--out`, or to standard output.
fn abi(args: &AbiArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let text = runtime.block_on(ship_archive::node_abi(&args.from))?;
    match &args.out {
        Some(path) => {
            std::fs::write(path, &text)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
            info!("wrote the ABI of {} to {}", args.from, path.display());
        }
        None => {
            let mut out = std::io::stdout().lock();
            out.write_all(text.as_bytes())?;
            out.flush()?;
        }
    }
    Ok(())
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
