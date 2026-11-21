//! `orrery-import`: copies a Moonraker database into orrery's store. See `orrery::import`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use orrery::config::Config;
use orrery::database::Database;
use orrery::import::{Counts, Options, Report};
use orrery::server::Paths;

#[derive(Parser)]
#[command(
    version,
    about = "Import a copy of Moonraker's database into orrery's store",
    long_about = "Import a copy of Moonraker's database into orrery's store.\n\n\
        Copy moonraker-sql.db while Moonraker is stopped, and stop orrery before you import.\n\
        The import only reads the copy. Running it again changes nothing."
)]
struct Cli {
    /// A copy of Moonraker's database, usually printer_data/database/moonraker-sql.db.
    #[arg(value_name = "moonraker-sql.db")]
    source: PathBuf,
    /// The printer's data folder, where orrery keeps its store.
    #[arg(short = 'd', long = "datapath", env = "MOONRAKER_DATA_PATH", value_name = "data path")]
    data_path: Option<PathBuf>,
    /// The config file. The default is config/moonraker.conf in the data folder.
    #[arg(short = 'c', long = "configfile", env = "MOONRAKER_CONFIG_PATH", value_name = "config file")]
    config_file: Option<PathBuf>,
    /// Show what the import would change, without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Remove everything earlier imports wrote, then import.
    #[arg(long)]
    replace: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orrery-import: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let data = orrery::expand_home(cli.data_path.as_deref().unwrap_or(Path::new("~/printer_data")))?;
    let config_file = match &cli.config_file {
        Some(path) => orrery::expand_home(path)?,
        None => data.join("config/moonraker.conf"),
    };
    let source = orrery::expand_home(&cli.source)?;
    let config = if config_file.exists() { Config::load(&config_file)? } else { Config::default() };
    let paths = Paths { data, config_file, log_file: None };
    let store = Database::open(&config, &paths).map_err(|e| {
        if format!("{e:#}").contains("already open") {
            anyhow::anyhow!("orrery's store is in use. Stop orrery, then run the import again.")
        } else {
            e
        }
    })?;
    let report = orrery::import::run(&store, &source, Options { dry_run: cli.dry_run, replace: cli.replace })
        .with_context(|| format!("can't import {}", source.display()))?;
    print_report(&report, cli.dry_run, &store.path().display().to_string());
    Ok(())
}

fn counts(c: &Counts) -> String {
    format!("{} added, {} updated, {} unchanged", c.added, c.updated, c.unchanged)
}

fn print_report(r: &Report, dry_run: bool, store: &str) {
    println!("Read {} (SHA-256 {}).", r.source, r.sha256);
    if r.replaced > 0 {
        println!("Removed {} rows that earlier imports wrote.", r.replaced);
    }
    println!("Settings: {}.", counts(&r.settings));
    if !r.skipped_namespaces.is_empty() {
        let names: Vec<String> = r.skipped_namespaces.iter().map(|(n, k)| format!("{n} ({k} keys)")).collect();
        println!("Skipped Moonraker's own namespaces: {}.", names.join(", "));
    }
    println!("Print history: {}.", counts(&r.history));
    for (old, new) in &r.moved_jobs {
        println!("Moved orrery's job {old:06X} to {new:06X}, to make room for Moonraker's job {old:06X}.");
    }
    println!("History totals: {}.", counts(&r.totals));
    if r.linked_files > 0 {
        println!("Linked {} files to their last print.", r.linked_files);
    }
    if r.api_key {
        println!("API key: copied. Clients that use Moonraker's API key keep working.");
    }
    if !r.user_names.is_empty() {
        println!("Users: {}: {}.", counts(&r.users), r.user_names.join(", "));
        println!("Their passwords and logins work as in Moonraker. orrery.toml decides their roles:");
        println!("without [authorization.users], every user is admin, as in Moonraker. To give them roles, add:");
        println!("  [authorization.users]");
        println!("  admin = [{}]", r.user_names.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", "));
    }
    for name in &r.ldap_users {
        println!("The user {name} logs in with LDAP, which orrery doesn't have yet, so they can't log in.");
    }
    for name in &r.kept_users {
        println!("Kept orrery's own user {name}. Moonraker's user of the same name wasn't imported.");
    }
    for (name, why) in &r.skipped_users {
        println!("Skipped the user {name}: {why}");
    }
    if dry_run {
        println!("Nothing was written, because of --dry-run.");
    } else {
        println!("Saved to {store}. Start orrery to use the imported data.");
    }
}
