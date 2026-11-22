//! orrery-updater: Moonraker's update manager as an orrery agent
//! (docs/design/2026-11-22-update-manager.md).
//!
//! orrery starts it when `[update_manager]`, or one of its entries, is in `orrery.toml` or, as
//! the fallback, in `moonraker.conf`. It answers Moonraker's `machine.update.*` methods, which
//! orrery maps to `agent.update_manager.*`, and sends `notify_update_refreshed` and
//! `notify_update_response`. It keeps each entry's state in orrery's database, in the
//! `update_manager` namespace.

mod config;
mod deps;
mod executable;
mod git;
mod manager;
mod packagekit;
mod plugins;
mod preflight;
mod release;
mod version;

/// Logs a warning with only the first line of `detail`, such as apt's first error, and the rest
/// in the verbose log. orrery writes each line of an agent's output as its own log line.
pub fn warn_first_line(what: &str, detail: &str) {
    let mut lines = detail.lines();
    tracing::warn!("{what}{}", lines.next().unwrap_or_default());
    if lines.next().is_some() {
        tracing::debug!("{what}{detail}");
    }
}

use std::process::ExitCode;

use clap::Parser;
use orrery_agent::{AgentBuilder, MethodError};

use manager::Manager;

/// orrery's update manager. orrery starts it as an agent. From a shell, it updates orrery or
/// rolls it back, even when orrery doesn't run.
#[derive(Parser)]
#[command(version = orrery_build::LONG_VERSION, args_conflicts_with_subcommands = true)]
struct Cli {
    /// The unix socket to reach orrery at, for tests. orrery gives its agents their socket.
    socket: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Install orrery's latest release over the programs in this folder. It takes effect when
    /// orrery restarts.
    SelfUpdate {
        /// Install this release instead, such as v0.2.0.
        #[arg(long = "version", value_name = "version")]
        tag: Option<String>,
        /// The repository to take releases from, as owner/name.
        #[arg(long, value_name = "owner/name")]
        repo: Option<String>,
        /// orrery's options, after --, for the new orrery's preflight check.
        #[arg(last = true, value_name = "orrery options")]
        orrery_options: Vec<String>,
    },
    /// Put back the programs the last update replaced, from .previous/. It takes effect when
    /// orrery restarts.
    Rollback {
        /// orrery's options, after --, for the old orrery's preflight check.
        #[arg(last = true, value_name = "orrery options")]
        orrery_options: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orrery-updater can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let done = match cli.command {
        None => {
            runtime.block_on(agent(cli.socket));
            return ExitCode::SUCCESS;
        }
        Some(Command::SelfUpdate { tag, repo, orrery_options }) => {
            runtime.block_on(release::self_update(repo, tag, &orrery_options))
        }
        Some(Command::Rollback { orrery_options }) => runtime.block_on(release::rollback(&orrery_options)),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn agent(socket: Option<String>) {
    orrery_agent::init_logging();
    let manager = Manager::new();
    let mut builder = AgentBuilder::new(config::SECTION).version(orrery_build::VERSION);
    if let Some(socket) = socket {
        builder = builder.socket(socket);
    }
    let m = manager.clone();
    builder = builder.method("status", move |params| {
        let m = m.clone();
        async move { m.status(&params).await }
    });
    let m = manager.clone();
    builder = builder.method("refresh", move |params| {
        let m = m.clone();
        async move { m.refresh(&params).await }
    });
    for name in ["upgrade", "client", "full"] {
        let m = manager.clone();
        builder = builder.method(name, move |params| {
            let m = m.clone();
            // `full` has no name: it updates everything.
            let params = if name == "full" { serde_json::Value::Null } else { params };
            async move { m.update(&params).await }
        });
    }
    let m = manager.clone();
    builder = builder.method("recover", move |params| {
        let m = m.clone();
        async move { m.recover(&params).await }
    });
    let m = manager.clone();
    builder = builder.method("rollback", move |params| {
        let m = m.clone();
        async move { m.rollback(&params).await }
    });
    let m = manager.clone();
    builder = builder.method("system", move |_| {
        let m = m.clone();
        async move { m.update(&serde_json::json!({"name": "system"})).await }
    });
    // Klipper's entry, named after the project Klippy runs (docs/design/2026-11-22-klipper-updates.md).
    let m = manager.clone();
    builder = builder.method("klipper", move |_| {
        let m = m.clone();
        async move {
            match m.klipper_entry_name() {
                Some(name) => m.update(&serde_json::json!({"name": name})).await,
                None => Err(MethodError::new(404, "Updater klipper not available")),
            }
        }
    });
    // Moonraker's own entry, which orrery doesn't have (see the update manager's design note).
    builder = builder.method("moonraker", |_| async {
        Err::<serde_json::Value, _>(MethodError::new(400, "orrery doesn't update Moonraker."))
    });
    orrery_agent::run(builder, move |agent| {
        let m = manager.clone();
        async move { m.setup(agent).await }
    })
    .await
}
