mod access;
mod api;
mod config;
mod data_store;
mod database;
mod files;
mod history;
mod job_queue;
mod job_state;
mod klippy;
mod logfile;
mod server;
mod web;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::config::Config;
use crate::server::{App, Paths};

#[derive(Parser)]
#[command(version, about = "Experimental Moonraker-compatible API server for Klipper")]
struct Cli {
    /// The printer's data folder.
    #[arg(short = 'd', long = "datapath", env = "MOONRAKER_DATA_PATH", value_name = "data path")]
    data_path: Option<PathBuf>,
    /// The config file. The default is config/moonraker.conf in the data folder.
    #[arg(short = 'c', long = "configfile", env = "MOONRAKER_CONFIG_PATH", value_name = "config file")]
    config_file: Option<PathBuf>,
    /// The log file. The default is logs/orrery.log in the data folder.
    #[arg(short = 'l', long = "logfile", value_name = "log file")]
    log_file: Option<PathBuf>,
    /// Log only to the terminal.
    #[arg(short = 'n', long = "nologfile", conflicts_with = "log_file")]
    no_log_file: bool,
    /// Log more detail.
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    match path.strip_prefix("~") {
        Ok(rest) => {
            let home = std::env::var_os("HOME").context("can't expand ~: HOME isn't set")?;
            Ok(PathBuf::from(home).join(rest))
        }
        Err(_) => Ok(path.to_path_buf()),
    }
}

fn resolve_paths(cli: &Cli) -> Result<Paths> {
    let data = expand_home(cli.data_path.as_deref().unwrap_or(Path::new("~/printer_data")))?;
    let config_file = match &cli.config_file {
        Some(path) => expand_home(path)?,
        None => data.join("config/moonraker.conf"),
    };
    let log_file = match &cli.log_file {
        _ if cli.no_log_file => None,
        Some(path) => Some(expand_home(path)?),
        None => Some(data.join("logs/orrery.log")),
    };
    Ok(Paths { data, config_file, log_file })
}

/// Logs to the terminal and, unless `-n` is given, to the log file. The returned guard flushes
/// the file when it's dropped. A log file that can't be opened becomes a warning.
fn init_logging(cli: &Cli, paths: &Paths) -> (Option<tracing_appender::non_blocking::WorkerGuard>, Option<String>) {
    use tracing_subscriber::prelude::*;

    let level = if cli.verbose { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into());
    let mut guard = None;
    let mut warning = None;
    let file_layer = paths.log_file.as_ref().and_then(|path| {
        let header = format!(
            "orrery {}\ndata path: {}\nconfig file: {}\nlog file: {}",
            env!("CARGO_PKG_VERSION"),
            paths.data.display(),
            paths.config_file.display(),
            path.display()
        );
        let opened =
            path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| logfile::LogFile::open(path, header));
        match opened {
            Ok(file) => {
                let (writer, g) = tracing_appender::non_blocking(file);
                guard = Some(g);
                Some(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer))
            }
            Err(e) => {
                warning = Some(format!(
                    "Unable to create log file at '{}': {e}. Make sure orrery can write to the folder '{}'.",
                    path.display(),
                    path.parent().unwrap_or(Path::new("")).display()
                ));
                None
            }
        }
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_ansi(false))
        .with(file_layer)
        .init();
    (guard, warning)
}

async fn run(paths: Paths, log_warning: Option<String>) -> Result<()> {
    for dir in [paths.data.clone(), paths.data.join("comms"), paths.logs_dir(), paths.gcodes_dir(), paths.config_dir()]
    {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let config = Config::load(&paths.config_file)?;
    let section = config.section("server");
    let host = section.get_str("host", "0.0.0.0");
    let port = section.get_int("port", 7125)?;
    drop(section);
    tracing::info!("orrery {} starting, data folder {}", env!("CARGO_PKG_VERSION"), paths.data.display());
    let app = App::new(config, paths)?;
    if let Some(warning) = log_warning {
        tracing::warn!("{warning}");
        app.add_warning(warning);
    }
    app.start();
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("listening on {addr}"))?;
    tracing::info!("Listening on {addr}");
    axum::serve(listener, web::router(app.clone())).with_graceful_shutdown(shutdown_signal()).await?;
    job_queue::on_exit(&app).await;
    history::on_exit(&app).await;
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("Shutting down");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = match resolve_paths(&cli) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let (_log_guard, log_warning) = init_logging(&cli, &paths);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(paths, log_warning)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            ExitCode::FAILURE
        }
    }
}
