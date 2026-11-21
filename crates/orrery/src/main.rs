use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use clap::Parser;

use orrery::config::Config;
use orrery::server::{App, Paths};
use orrery::{agent_process, expand_home, history, job_queue, logfile, unix_socket, web};

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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Show or rotate the API keys that orrery.toml declares. orrery must be running: the
    /// command asks it over the unix socket.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
}

#[derive(clap::Subcommand)]
enum KeyAction {
    /// Print a key, such as homeassistant, agent.spoolman, or moonraker.
    Show { name: String },
    /// Give a key a new secret, and print it. The old secret stops working at once.
    Rotate { name: String },
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
                let (writer, g) = tracing_appender::non_blocking(file.share());
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
    // The journal records when each line came, so lines that go there leave out the time.
    let journal = stdout_is_journal();
    tracing_subscriber::registry()
        .with(filter)
        .with((!journal).then(|| tracing_subscriber::fmt::layer().with_ansi(false)))
        .with(journal.then(|| tracing_subscriber::fmt::layer().event_format(JournalFormat)))
        .with(file_layer)
        .init();
    (guard, warning)
}

/// Lines for the journal, such as `<4>WARN orrery::web: …`. journald takes the `<4>` as the
/// line's priority and adds the time itself.
struct JournalFormat;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for JournalFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let priority = match *meta.level() {
            tracing::Level::ERROR => 3,
            tracing::Level::WARN => 4,
            tracing::Level::INFO => 6,
            _ => 7,
        };
        write!(writer, "<{priority}>{} {}: ", meta.level(), meta.target())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Whether stdout goes to the systemd journal. systemd sets `JOURNAL_STREAM` to the device and
/// inode of that stream; a child process with other output inherits the variable too, so it's
/// compared with stdout.
fn stdout_is_journal() -> bool {
    let Some((dev, ino)) = std::env::var("JOURNAL_STREAM").ok().and_then(|s| {
        let (dev, ino) = s.split_once(':')?;
        Some((dev.parse::<u64>().ok()?, ino.parse::<u64>().ok()?))
    }) else {
        return false;
    };
    // The stat fields' types differ between targets.
    #[allow(clippy::unnecessary_cast)]
    rustix::fs::fstat(std::io::stdout()).is_ok_and(|st| st.st_dev as u64 == dev && st.st_ino as u64 == ino)
}

/// Where `orrery.toml` is, and whether it must exist.
fn orrery_config_path(cli: &Cli, paths: &Paths) -> Result<(PathBuf, bool)> {
    match &cli.orrery_config {
        Some(path) => Ok((expand_home(path)?, true)),
        None => Ok((paths.config_file.with_file_name("orrery.toml"), false)),
    }
}

/// Runs orrery until it's stopped. Returns whether a client asked it to restart.
async fn run(paths: Paths, socket_path: PathBuf, toml: (PathBuf, bool), log_warning: Option<String>) -> Result<bool> {
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
    let agents = agent_process::start(&app, _socket.as_ref().map(|_| socket_path.as_path()));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("listening on {addr}"))?;
    tracing::info!("Listening on {addr}");
    let service = web::router(app.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let restart = Arc::new(AtomicBool::new(false));
    axum::serve(listener, service).with_graceful_shutdown(shutdown_signal(app.clone(), restart.clone())).await?;
    agents.stop().await;
    job_queue::on_exit(&app).await;
    history::on_exit(&app).await;
    Ok(restart.load(Ordering::Relaxed))
}

async fn shutdown_signal(app: Arc<App>, restart: Arc<AtomicBool>) {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = app.restart.notified() => restart.store(true, Ordering::Relaxed),
    }
    tracing::info!("Shutting down");
}

/// Starts orrery's program again in this process, with the same arguments. It's started by
/// the name it was run with, not `/proc/self/exe`, so a binary copied over the old one runs.
fn restart() -> ExitCode {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_else(|| "orrery".into());
    let e = std::process::Command::new(&program).args(args).exec();
    eprintln!("orrery can't restart: running {} failed: {e}", Path::new(&program).display());
    ExitCode::FAILURE
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
    if let Some(Command::Key { action }) = &cli.command {
        return key_command(&socket_path, action);
    }
    let (_log_guard, log_warning) = init_logging(&cli, &paths);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(paths, socket_path, toml, log_warning));
    // Ends the tasks that are left, and writes out the log, before a restart replaces the
    // process.
    drop(runtime);
    drop(_log_guard);
    match result {
        Ok(true) => restart(),
        Ok(false) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `orrery key show` and `orrery key rotate`: one request to the running orrery, over the unix
/// socket, where orrery's own user is admin.
fn key_command(socket: &Path, action: &KeyAction) -> ExitCode {
    let (method, name) = match action {
        KeyAction::Show { name } => ("orrery.access.key.get", name),
        KeyAction::Rotate { name } => ("orrery.access.key.rotate", name),
    };
    match unix_socket::call(socket, method, serde_json::json!({"name": name})) {
        Ok(result) => {
            println!("{}", result["key"].as_str().unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("orrery key: {e:#}");
            ExitCode::FAILURE
        }
    }
}
