mod api;
mod config;
mod data_store;
mod database;
mod job_state;
mod klippy;
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
    Ok(Paths { data, config_file })
}

async fn run(cli: Cli) -> Result<()> {
    let paths = resolve_paths(&cli)?;
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
    app.start();
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("listening on {addr}"))?;
    tracing::info!("Listening on {addr}");
    axum::serve(listener, web::router(app)).with_graceful_shutdown(shutdown_signal()).await?;
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
    let level = if cli.verbose { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into());
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).init();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            ExitCode::FAILURE
        }
    }
}
