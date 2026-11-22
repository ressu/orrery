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
#[command(version = orrery_build::LONG_VERSION, about = "Experimental Moonraker-compatible API server for Klipper")]
struct Cli {
    /// The printer's data folder.
    #[arg(short = 'd', long = "datapath", env = "MOONRAKER_DATA_PATH", value_name = "data path", global = true)]
    data_path: Option<PathBuf>,
    /// The config file. The default is config/moonraker.conf in the data folder.
    #[arg(short = 'c', long = "configfile", env = "MOONRAKER_CONFIG_PATH", value_name = "config file", global = true)]
    config_file: Option<PathBuf>,
    /// orrery's own config, which wins over the config file. The default is orrery.toml next to
    /// the config file. It's optional, unless this option names it.
    #[arg(long = "orrery-config", env = "ORRERY_CONFIG", value_name = "orrery.toml", global = true)]
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
    /// List, show, or rotate the API keys that orrery.toml declares. orrery must be running: the
    /// command asks it over the unix socket.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// List, add, or delete local users, or set their passwords. Roles come from orrery.toml's
    /// [authorization.users]. orrery must be running: the command asks it over the unix socket.
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Check that this orrery can start with the config and the database, without starting it.
    /// An update runs it from the new version, before the new version takes effect.
    Preflight,
    /// Update orrery to its latest release. The running orrery does the update and restarts;
    /// when it doesn't answer, orrery-updater replaces the programs, and orrery runs them once
    /// it restarts.
    SelfUpdate {
        /// Install this release instead, such as v0.2.0. orrery-updater installs it.
        #[arg(long = "version", value_name = "version")]
        tag: Option<String>,
        /// The repository to take the release from, as owner/name. orrery-updater installs it.
        #[arg(long, value_name = "owner/name")]
        repo: Option<String>,
    },
    /// Put back the programs the last update replaced, from .previous/. The running orrery
    /// restarts with them; when it doesn't answer, they take effect once it restarts.
    Rollback,
    /// Run with sudo. Writes orrery.service, the orrery-admin group, and a polkit rule that lets
    /// orrery.service manage services, reboot, and shut down. Run it again after an upgrade.
    /// -d, -c, and --orrery-config go into the service; without them, the installed ones stay.
    Setup(SetupArgs),
}

#[derive(clap::Args)]
struct SetupArgs {
    /// Report what's in place, and change nothing. It works without sudo.
    #[arg(long, conflicts_with_all = ["remove", "disable_moonraker", "start", "migrate"])]
    check: bool,
    /// Stop and disable orrery.service, then remove the unit, the polkit rule, and the group.
    #[arg(long, conflicts_with_all = ["strict", "disable_moonraker", "start", "migrate"])]
    remove: bool,
    /// Let orrery manage only its allowed services: moonraker.asvc, Klipper's and orrery's own,
    /// and agents' units. Run setup again after adding one.
    #[arg(long)]
    strict: bool,
    /// Stop and disable moonraker.service.
    #[arg(long)]
    disable_moonraker: bool,
    /// Enable orrery.service and start it, or restart it if it runs.
    #[arg(long)]
    start: bool,
    /// Switch from Moonraker to orrery: --disable-moonraker and --start.
    #[arg(long)]
    migrate: bool,
}

#[derive(clap::Subcommand)]
enum UserAction {
    /// List the users, with their roles from orrery.toml.
    List,
    /// Add a user. The command asks for the password.
    Add { name: String },
    /// Delete a user. Their sessions end at once.
    Delete { name: String },
    /// Set a user's password. Their sessions end at once.
    Password { name: String },
}

#[derive(clap::Subcommand)]
enum KeyAction {
    /// List the keys and their roles, without their secrets.
    List,
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
/// How many lines may wait for the log file's writer.
const LOG_BUFFER_LINES: usize = 4096;

fn init_logging(cli: &Cli, paths: &Paths) -> (Option<tracing_appender::non_blocking::WorkerGuard>, Option<String>) {
    use tracing_subscriber::prelude::*;

    let level = if cli.verbose { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into());
    let mut guard = None;
    let mut warning = None;
    let file_layer = paths.log_file.as_ref().and_then(|path| {
        let header = format!(
            "orrery {}\ndata path: {}\nconfig file: {}\nlog file: {}",
            orrery_build::LONG_VERSION,
            paths.data.display(),
            paths.config_file.display(),
            path.display()
        );
        let opened =
            path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| logfile::LogFile::open(path, header));
        match opened {
            Ok(file) => {
                // tracing-appender's default buffer, 128,000 lines, takes 4 MB as soon as it's made.
                // Lines past the limit are dropped, not waited for.
                let (writer, g) = tracing_appender::non_blocking::NonBlockingBuilder::default()
                    .buffered_lines_limit(LOG_BUFFER_LINES)
                    .finish(file.share());
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

fn setup_command(cli: &Cli, args: &SetupArgs) -> ExitCode {
    let euid = rustix::process::geteuid();
    let exe = match std::env::current_exe().and_then(|e| e.canonicalize()) {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("orrery can't set up: it can't tell where its own binary is: {e}");
            return ExitCode::FAILURE;
        }
    };
    let options = orrery::setup::Options {
        check: args.check,
        remove: args.remove,
        strict: args.strict,
        disable_moonraker: args.disable_moonraker || args.migrate,
        start: args.start || args.migrate,
        data_path: cli.data_path.clone(),
        config_file: cli.config_file.clone(),
        orrery_config: cli.orrery_config.clone(),
        sudo_user: std::env::var("SUDO_USER").ok(),
        is_root: euid.is_root(),
        uid: euid.as_raw(),
        exe,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    };
    match orrery::setup::run(&options, Path::new("/"), &mut orrery::setup::System) {
        Ok(report) => {
            for line in &report.lines {
                println!("{line}");
            }
            if report.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("orrery can't set up: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs orrery until it's stopped. Returns whether a client asked it to restart.
async fn run(
    paths: Paths,
    socket_path: PathBuf,
    toml: (PathBuf, bool),
    log_warning: Option<String>,
    verbose: bool,
) -> Result<bool> {
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
    tracing::info!("orrery {} starting, data folder {}", orrery_build::LONG_VERSION, paths.data.display());
    let app = App::new(config, paths)?;
    if let Some(warning) = log_warning {
        tracing::warn!("{warning}");
        app.add_warning(warning);
    }
    app.start();
    // Kept until orrery exits, which removes the socket file.
    let _socket = unix_socket::start(&app, &socket_path).await?;
    let agents = agent_process::start(&app, _socket.as_ref().map(|_| socket_path.as_path()), verbose);
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
    if let Some(Command::Setup(args)) = &cli.command {
        return setup_command(&cli, args);
    }
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
    match &cli.command {
        Some(Command::Key { action }) => return key_command(&socket_path, action),
        Some(Command::User { action }) => return user_command(&socket_path, action),
        Some(Command::Preflight) => {
            let report = orrery::preflight::run(&paths, (&toml.0, toml.1));
            for line in &report.lines {
                println!("{line}");
            }
            return if report.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        Some(Command::SelfUpdate { tag, repo }) => {
            return self_update_command(&cli, &socket_path, false, tag.as_deref(), repo.as_deref());
        }
        Some(Command::Rollback) => return self_update_command(&cli, &socket_path, true, None, None),
        Some(Command::Setup(_)) | None => {}
    }
    let (_log_guard, log_warning) = init_logging(&cli, &paths);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(paths, socket_path, toml, log_warning, cli.verbose));
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

/// How long `orrery self-update` waits for the running orrery's update: a download on a slow
/// connection, and the preflight.
const SELF_UPDATE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// `orrery self-update` and `orrery rollback`: asks the running orrery, which checks for a
/// print and restarts, or runs orrery-updater beside this program when orrery doesn't answer,
/// or has no update manager. `--version` and `--repo` always go to orrery-updater.
fn self_update_command(cli: &Cli, socket: &Path, rollback: bool, tag: Option<&str>, repo: Option<&str>) -> ExitCode {
    let (method, application) =
        if rollback { ("machine.update.rollback", "rollback_orrery") } else { ("machine.update.upgrade", "orrery") };
    if tag.is_none() && repo.is_none() {
        let (mut said, mut complete) = (Vec::<String>::new(), false);
        let watched = unix_socket::call_watching(
            socket,
            method,
            serde_json::json!({"name": "orrery"}),
            SELF_UPDATE_LIMIT,
            |event, params| {
                let message = &params[0];
                if event == "notify_update_response" && message["application"] == application {
                    let line = message["message"].as_str().unwrap_or_default().to_owned();
                    println!("{line}");
                    said.push(line);
                    complete |= message["complete"] == true;
                }
            },
        );
        match watched {
            // orrery restarts at the end, which may close the connection before the answer.
            Ok(unix_socket::Watched::Closed) if !complete => {
                eprintln!("orrery closed the connection before the update finished. Its log says why.");
                return ExitCode::FAILURE;
            }
            Ok(unix_socket::Watched::Answered(_) | unix_socket::Watched::Closed) => {
                if said.is_empty() {
                    println!("orrery {} is the latest release.", orrery_build::VERSION);
                }
                return ExitCode::SUCCESS;
            }
            // No update manager, so no method or no entry for orrery.
            Ok(unix_socket::Watched::Refused(404 | -32601, message)) => {
                println!("orrery has no update manager running ({message}), so orrery-updater does it without orrery.");
            }
            Ok(unix_socket::Watched::Refused(_, message)) => {
                // The update dialog's messages usually said it already.
                if !said.iter().any(|line| line.ends_with(&message)) {
                    eprintln!("{message}");
                }
                return ExitCode::FAILURE;
            }
            Err(e) => println!("orrery doesn't answer ({e:#}), so orrery-updater does it without orrery."),
        }
    }
    let dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_default();
    let updater = dir.join("orrery-updater");
    let mut command = std::process::Command::new(&updater);
    command.arg(if rollback { "rollback" } else { "self-update" });
    if let Some(tag) = tag {
        command.args(["--version", tag]);
    }
    if let Some(repo) = repo {
        command.args(["--repo", repo]);
    }
    command.arg("--");
    // orrery's own options, for the preflight, which runs in another folder.
    let cwd = std::env::current_dir().unwrap_or_default();
    for (flag, path) in [("-d", &cli.data_path), ("-c", &cli.config_file), ("--orrery-config", &cli.orrery_config)] {
        if let Some(path) = path {
            let path = expand_home(path).unwrap_or_else(|_| path.clone());
            command.arg(flag).arg(cwd.join(path));
        }
    }
    let e = command.exec();
    eprintln!("orrery can't run {}: {e}", updater.display());
    ExitCode::FAILURE
}

/// `orrery key list|show|rotate`: one request to the running orrery, over the unix socket,
/// where orrery's own user is admin.
fn key_command(socket: &Path, action: &KeyAction) -> ExitCode {
    let (method, name) = match action {
        KeyAction::List => ("orrery.access.key.list", None),
        KeyAction::Show { name } => ("orrery.access.key.get", Some(name)),
        KeyAction::Rotate { name } => ("orrery.access.key.rotate", Some(name)),
    };
    let params = name.map_or_else(|| serde_json::json!({}), |n| serde_json::json!({"name": n}));
    match unix_socket::call(socket, method, params) {
        Ok(result) if name.is_none() => {
            let keys = result["keys"].as_array().cloned().unwrap_or_default();
            if keys.is_empty() {
                println!("No keys. Keys are listed by role in orrery.toml's [authorization.keys].");
                return ExitCode::SUCCESS;
            }
            let rows: Vec<Vec<String>> = keys
                .iter()
                .map(|k| {
                    let name = k["name"].as_str().unwrap_or_default();
                    let note = if name == "moonraker" {
                        "Moonraker's API key; [moonraker_compat] legacy_api_key = false turns it off"
                    } else {
                        ""
                    };
                    vec![name.to_owned(), k["role"].as_str().unwrap_or_default().to_owned(), note.to_owned()]
                })
                .collect();
            print_table(&["NAME", "ROLE", ""], &rows);
            ExitCode::SUCCESS
        }
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

/// `orrery user add|delete|password`: one request to the running orrery, over the unix socket,
/// where orrery's own user is admin. A password is read from the terminal, never taken as an
/// argument, so it stays out of the shell's history.
fn user_command(socket: &Path, action: &UserAction) -> ExitCode {
    let result = (|| -> Result<String> {
        Ok(match action {
            UserAction::List => {
                let result = unix_socket::call(socket, "orrery.access.user.list", serde_json::json!({}))?;
                let users = result["users"].as_array().cloned().unwrap_or_default();
                if users.is_empty() {
                    return Ok("No users. Add one with: orrery user add <name>".to_owned());
                }
                let rows: Vec<Vec<String>> = users
                    .iter()
                    .map(|u| {
                        vec![
                            u["username"].as_str().unwrap_or_default().to_owned(),
                            u["source"].as_str().unwrap_or_default().to_owned(),
                            u["role"].as_str().unwrap_or("none: not in [authorization.users]").to_owned(),
                            u["created_on"].as_f64().map(date).unwrap_or_default(),
                            if u["has_tokens"].as_bool() == Some(true) { "yes" } else { "no" }.to_owned(),
                        ]
                    })
                    .collect();
                print_table(&["NAME", "SOURCE", "ROLE", "ADDED", "TOKENS"], &rows);
                return Ok(String::new());
            }
            UserAction::Add { name } => {
                let password = read_new_password()?;
                unix_socket::call(
                    socket,
                    "access.post_user",
                    serde_json::json!({"username": name, "password": password}),
                )?;
                format!("Added the user {name}. Their role comes from orrery.toml's [authorization.users].")
            }
            UserAction::Delete { name } => {
                unix_socket::call(socket, "access.delete_user", serde_json::json!({"username": name}))?;
                format!("Deleted the user {name}.")
            }
            UserAction::Password { name } => {
                let password = read_new_password()?;
                unix_socket::call(
                    socket,
                    "orrery.access.user.password",
                    serde_json::json!({"username": name, "password": password}),
                )?;
                format!("Set the password of {name}. Their other sessions have ended.")
            }
        })
    })();
    match result {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("orrery user: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Asks for a new password twice, without showing it.
fn read_new_password() -> Result<String> {
    let first = read_hidden("Password: ")?;
    if first.is_empty() {
        anyhow::bail!("the password is empty");
    }
    if read_hidden("The same password again: ")? != first {
        anyhow::bail!("the two passwords differ");
    }
    Ok(first)
}

/// Reads a line from the terminal with echo off.
fn read_hidden(prompt: &str) -> Result<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let old = tcgetattr(&stdin).map_err(|_| anyhow::anyhow!("the password has to be typed in a terminal"))?;
    let mut quiet = old.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    tcsetattr(&stdin, OptionalActions::Now, &quiet)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    tcsetattr(&stdin, OptionalActions::Now, &old)?;
    eprintln!();
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

/// Prints rows in columns, under a header.
fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> = cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

/// A Unix time as a date, `2026-11-21`, in UTC.
fn date(secs: f64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let days = (secs / 86400.0).floor() as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(0.0), "1970-01-01");
        assert_eq!(super::date(1_795_219_200.0), "2026-11-21");
        assert_eq!(super::date(951_782_400.0), "2000-02-29");
    }
}
