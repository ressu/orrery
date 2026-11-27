//! orrery: an experimental Moonraker-compatible API server for Klipper.
//!
//! The `orrery` binary runs the server. `orrery-import` copies a Moonraker database into
//! orrery's store.

mod access;
mod actions;
pub mod agent_process;
mod agents;
mod api;
mod auth;
mod buttons;
mod compat;
pub mod config;
mod data_store;
pub mod database;
mod extensions;
mod files;
pub mod history;
pub mod import;
pub mod job_queue;
mod job_state;
mod keys;
mod klippy;
pub mod logfile;
mod machine;
mod octoprint;
pub mod preflight;
mod proc_stats;
pub mod server;
mod services;
pub mod setup;
pub mod unix_socket;
mod users;
pub mod web;
mod webcam;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Expands a leading `~` to the home folder.
pub fn expand_home(path: &Path) -> Result<PathBuf> {
    match path.strip_prefix("~") {
        Ok(rest) => {
            let home = std::env::var_os("HOME").context("can't expand ~: HOME isn't set")?;
            Ok(PathBuf::from(home).join(rest))
        }
        Err(_) => Ok(path.to_path_buf()),
    }
}
