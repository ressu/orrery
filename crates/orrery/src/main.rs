use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use orrery::config::Config;
use orrery::server::{App, Paths};
use orrery::{expand_home, history, job_queue, logfile, unix_socket, web};

#[derive(Parser)]
#[command(version, about = "Experimental Moonraker-compatible API server for Klipper")]
struct Cli {
    /// The printer's data folder.
    #[arg(short = 'd', long = "datapath", env = "MOONRAKER_DATA_PATH", value_name = "data path")]
    data_path: Option<PathBuf>,
    /// The config file. The default is config/moonraker.conf in the data folder.
    #[arg(short = 'c', long = "configfile", env = "MOONRAKER_CONFIG_PATH", value_name = "config file")]
    config_file: Option<PathBuf>,
    /// orrery's own config, which wins over the config file. The default is orrery.toml next to
    /// the config file. It's optional, unless this option names it.
    #[arg(long = "orrery-config", env = "ORRERY_CONFIG", value_name = "orrery.toml")]
    orrery_config: Option<PathBuf>,
    /// The log file. The default is logs/orrery.log in the data folder.
    #[arg(short = 'l', long = "logfile", value_name = "log file")]
    log_file: Option<PathBuf>,
    /// The unix socket for local clients and agents. The default is comms/moonraker.sock in the
    /// data folder.
    #[arg(short = 'u', long = "unixsocket", env = "MOONRAKER_UDS_PATH", value_name = "unix socket")]
    unix_socket: Option<PathBuf>,
    /// Log only to the terminal.
    #[arg(short = 'n', long = "nologfile", conflicts_with = "log_file")]
    no_log_file: bool,
    /// Log more detail.
    #[arg(short = 'v', long)]
    verbose: bool,
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

/// Where `orrery.toml` is, and whether it must exist.
fn orrery_config_path(cli: &Cli, paths: &Paths) -> Result<(PathBuf, bool)> {
    match &cli.orrery_config {
        Some(path) => Ok((expand_home(path)?, true)),
        None => Ok((paths.config_file.with_file_name("orrery.toml"), false)),
    }
}

async fn run(paths: Paths, socket_path: PathBuf, toml: (PathBuf, bool), log_warning: Option<String>) -> Result<()> {
    for dir in [paths.data.clone(), paths.data.join("comms"), paths.logs_dir(), paths.gcodes_dir(), paths.config_dir()]
    {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut config = Config::load(&paths.config_file)?;
    let (toml_path, required) = toml;
    if required || toml_path.exists() {
        config.load_toml(&toml_path)?;
        tracing::info!("Read orrery's config from {}", toml_path.display());
    }
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
    // Kept until orrery exits, which removes the socket file.
    let _socket = unix_socket::start(&app, &socket_path).await?;
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("listening on {addr}"))?;
    tracing::info!("Listening on {addr}");
    let service = web::router(app.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, service).with_graceful_shutdown(shutdown_signal()).await?;
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
    let socket_path = match &cli.unix_socket {
        Some(path) => expand_home(path),
        None => Ok(paths.data.join("comms/moonraker.sock")),
    };
    let toml = match orrery_config_path(&cli, &paths) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let socket_path = match socket_path {
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
    match runtime.block_on(run(paths, socket_path, toml, log_warning)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            ExitCode::FAILURE
        }
    }
}
