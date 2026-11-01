//! Black-box conformance suite for Moonraker-compatible servers.
//!
//! Cases are YAML files under `conformance/cases/`. Each runs against a fresh server (upstream
//! Moonraker as the reference, or orrery) with a fake Klippy, once per transport. See
//! `conformance/README.md` for the case format.

pub mod case;
pub mod client;
pub mod matcher;
pub mod runner;
pub mod snapshot;
pub mod target;
