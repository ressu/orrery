use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

/// The command line matches Moonraker's (`moonraker/server.py`), including its environment
/// variables, so orrery can start from an existing moonraker.service unit unchanged.
#[derive(Parser)]
#[command(version, about = "Experimental Moonraker-compatible API server for Klipper")]
struct Cli {
    /// The printer's data folder.
    #[arg(short = 'd', long = "datapath", env = "MOONRAKER_DATA_PATH", value_name = "data path")]
    data_path: Option<PathBuf>,
    /// The config file. The default is config/moonraker.conf in the data folder.
    #[arg(short = 'c', long = "configfile", env = "MOONRAKER_CONFIG_PATH", value_name = "config file")]
    config_file: Option<PathBuf>,
    /// The log file.
    #[arg(short = 'l', long = "logfile", env = "MOONRAKER_LOG_PATH", value_name = "log file")]
    log_file: Option<PathBuf>,
    /// The API's unix socket. The default is comms/moonraker.sock in the data folder.
    #[arg(short = 'u', long = "unixsocket", env = "MOONRAKER_UDS_PATH", value_name = "unix socket")]
    unix_socket: Option<PathBuf>,
    /// Don't log to a file.
    #[arg(short = 'n', long = "nologfile", env = "MOONRAKER_DISABLE_FILE_LOG")]
    no_log_file: bool,
    /// Log in a structured format. Accepted for compatibility; it has no effect yet.
    #[arg(short = 's', long = "structured-logging", env = "MOONRAKER_STRUCTURED_LOGGING")]
    structured_logging: bool,
    /// Log more detail.
    #[arg(short = 'v', long, env = "MOONRAKER_VERBOSE_LOGGING")]
    verbose: bool,
    /// Turn on debug features.
    #[arg(short = 'g', long, env = "MOONRAKER_ENABLE_DEBUG")]
    debug: bool,
    /// Moonraker's asyncio debug flag. Accepted for compatibility; it has no effect.
    #[arg(short = 'o', long = "asyncio-debug", env = "MOONRAKER_ASYNCIO_DEBUG", hide = true)]
    asyncio_debug: bool,
}

/// Where orrery reads and writes, resolved the way Moonraker resolves them.
struct Paths {
    data: PathBuf,
    config_file: PathBuf,
    unix_socket: PathBuf,
}

impl Paths {
    fn resolve(cli: &Cli) -> Result<Self> {
        let data = match &cli.data_path {
            Some(path) => expand_home(path)?,
            None => expand_home(Path::new("~/printer_data"))?,
        };
        let config_file = match &cli.config_file {
            Some(path) => expand_home(path)?,
            None => data.join("config/moonraker.conf"),
        };
        let unix_socket = match &cli.unix_socket {
            Some(path) => expand_home(path)?,
            None => data.join("comms/moonraker.sock"),
        };
        Ok(Self { data, config_file, unix_socket })
    }
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

fn describe(path: &Path) -> String {
    if path.exists() { path.display().to_string() } else { format!("{} (not found)", path.display()) }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = match Paths::resolve(&cli) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("orrery can't start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    println!("orrery {} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH);
    println!("Data folder: {}", describe(&paths.data));
    println!("Config file: {}", describe(&paths.config_file));
    println!("Unix socket: {}", paths.unix_socket.display());
    eprintln!("orrery can't serve the API yet: the server isn't written.");
    ExitCode::FAILURE
}
