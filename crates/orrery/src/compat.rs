//! `[moonraker_compat]`: what orrery does only so that clients written for Moonraker work.
//!
//! Compatibility is on when orrery reads a `moonraker.conf` or `orrery.toml` has the section,
//! unless `enabled = false`. Each feature can be turned off on its own (user, 2026-11-21):
//!
//! ```toml
//! [moonraker_compat]
//! legacy_api_key = "admin"  # Moonraker's one API key and its role; false: no such key
//! user_source_fix = true    # Moonraker's replies call local users' source "moonraker"
//! service_alias = true      # `orrery setup` makes moonraker.service a name for orrery.service
//! remote_methods = true     # Moonraker's remote methods for Klipper's macros, as actions
//! klipper_shutdown = true   # shutdown_machine and reboot_machine run as admin from Klipper
//! moonraker_version = "v0.9.0"  # every client's moonraker_version; unset: Mobileraker's only
//! ```
//!
//! OctoPrint's compatibility will get a section of its own.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::auth::Role;
use crate::config::Config;

/// orrery's name for users whose password it checks.
pub const LOCAL_SOURCE: &str = "local";

/// Moonraker's name for them, which Fluidd needs to offer deleting a user or changing a
/// password (`fluidd/src/components/settings/auth/AuthSettings.vue:61`).
const MOONRAKER_SOURCE: &str = "moonraker";

/// The Moonraker version Orrery tells Mobileraker it is. Mobileraker reads cameras from
/// Moonraker's webcam API only from v0.8.0-80, and reads `orrery-v0.2.0` as 0.0.0
/// (`mobileraker/common/lib/data/repository/webcam_info_repository.dart`). Orrery's own
/// version follows, so Mobileraker's parser keeps the first three parts (user, 2026-11-27).
const MOBILERAKER_VERSION: &str = "v0.9.0-0-orrery";

/// What `server.info` reports as `moonraker_version`.
#[derive(Debug, Clone, PartialEq)]
pub enum ReportedVersion {
    /// Orrery's own version to every client.
    Orrery,
    /// A Moonraker version to Mobileraker, which identifies as `Mobileraker-<os>`, and
    /// Orrery's own to the others.
    Mobileraker,
    /// This, as written, to every client.
    Always(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MoonrakerCompat {
    pub enabled: bool,
    /// The role of Moonraker's one API key, the key named `moonraker`. `None`: no such key.
    pub api_key: Option<Role>,
    /// Moonraker's replies call local users' source `moonraker`, which Fluidd needs.
    pub user_source_fix: bool,
    /// `orrery setup` makes `moonraker.service` a name for `orrery.service`, so units that start
    /// or wait for Moonraker get orrery.
    pub service_alias: bool,
    /// Moonraker's remote methods, such as `shutdown_machine`, registered with Klipper, each
    /// running an action (docs/design/2026-11-23-buttons-and-actions.md).
    pub remote_methods: bool,
    /// `shutdown_machine` and `reboot_machine` from Klipper run as `admin`, as in Moonraker,
    /// where any macro may call them.
    pub klipper_shutdown: bool,
    /// The `moonraker_version` that `server.info` reports.
    pub moonraker_version: ReportedVersion,
}

impl MoonrakerCompat {
    pub fn new(config: &Config) -> Result<Self> {
        if config.toml_has("authorization", "enable_api_key") {
            bail!(
                "orrery.toml's [authorization] enable_api_key is now [moonraker_compat] legacy_api_key, which takes \
                 the key's role, such as legacy_api_key = \"admin\", or false for no key"
            );
        }
        let table = config.section_json("moonraker_compat");
        let enabled = match table.as_ref().and_then(|t| t.get("enabled")) {
            None => config.read_moonraker_conf() || table.is_some(),
            Some(Value::Bool(on)) => *on,
            Some(other) => bail!("[moonraker_compat] enabled: {other} isn't true or false"),
        };
        if !enabled {
            return Ok(Self {
                enabled,
                api_key: None,
                user_source_fix: false,
                service_alias: false,
                remote_methods: false,
                klipper_shutdown: false,
                moonraker_version: ReportedVersion::Orrery,
            });
        }
        let api_key = match table.as_ref().and_then(|t| t.get("legacy_api_key")) {
            // Moonraker's own option, from moonraker.conf: on unless it's turned off.
            None => config.section("authorization").get_bool("enable_api_key", true)?.then_some(Role::Admin),
            Some(Value::Bool(false)) => None,
            Some(Value::String(role)) => Some(Role::parse(role).ok_or_else(|| {
                anyhow::anyhow!(
                    "[moonraker_compat] legacy_api_key: {role:?} isn't a role. Use viewer, operator, or admin, or false for \
                     no key"
                )
            })?),
            Some(other) => bail!(
                "[moonraker_compat] legacy_api_key: {other} isn't a role. Use a role such as \"admin\", or false for no \
                 key"
            ),
        };
        let flag = |key: &str| match table.as_ref().and_then(|t| t.get(key)) {
            None => Ok(true),
            Some(Value::Bool(on)) => Ok(*on),
            Some(other) => Err(anyhow::anyhow!("[moonraker_compat] {key}: {other} isn't true or false")),
        };
        let moonraker_version = match table.as_ref().and_then(|t| t.get("moonraker_version")) {
            None | Some(Value::Bool(true)) => ReportedVersion::Mobileraker,
            Some(Value::Bool(false)) => ReportedVersion::Orrery,
            Some(Value::String(v)) if !v.trim().is_empty() => ReportedVersion::Always(v.clone()),
            Some(other) => bail!(
                "[moonraker_compat] moonraker_version: {other} isn't a version. Use one such as \"v0.9.0-0-orrery\", \
                 or false for Orrery's own"
            ),
        };
        Ok(Self {
            moonraker_version,
            enabled,
            api_key,
            user_source_fix: flag("user_source_fix")?,
            service_alias: flag("service_alias")?,
            remote_methods: flag("remote_methods")?,
            klipper_shutdown: flag("klipper_shutdown")?,
        })
    }

    /// `server.info`'s `moonraker_version` for a client that identified as `client` in
    /// `server.connection.identify`.
    pub fn moonraker_version(&self, client: Option<&str>) -> String {
        let own = || format!("orrery-{}", orrery_build::VERSION);
        match &self.moonraker_version {
            ReportedVersion::Always(v) => v.clone(),
            ReportedVersion::Mobileraker if client.is_some_and(is_mobileraker) => {
                format!("{MOBILERAKER_VERSION}-{}", orrery_build::VERSION)
            }
            _ => own(),
        }
    }

    /// The name of a user's source in Moonraker's replies.
    pub fn source_name<'a>(&self, source: &'a str) -> &'a str {
        if source == LOCAL_SOURCE && self.user_source_fix { MOONRAKER_SOURCE } else { source }
    }

    /// The source a client names in `access.login`, as orrery calls it.
    pub fn parse_source(&self, name: &str) -> String {
        if name == self.source_name(LOCAL_SOURCE) { LOCAL_SOURCE.to_owned() } else { name.to_owned() }
    }

    pub fn log_settings(&self) {
        if !self.enabled {
            tracing::info!(target: "orrery::auth", "Moonraker compatibility: off ([moonraker_compat] enabled = false)");
            return;
        }
        let key = match self.api_key {
            Some(role) => format!("API key moonraker ({role})"),
            None => "no API key".to_owned(),
        };
        tracing::info!(
            target: "orrery::auth",
            "Moonraker compatibility: {key}; local users show as source {}",
            self.source_name(LOCAL_SOURCE)
        );
    }
}

/// Mobileraker's app names itself `Mobileraker-android`, `Mobileraker-ios`, and so on
/// (`mobileraker/common/lib/network/json_rpc_client.dart`).
fn is_mobileraker(client: &str) -> bool {
    client.get(..11).is_some_and(|p| p.eq_ignore_ascii_case("mobileraker"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compat(moonraker: Option<&str>, toml: &str) -> Result<MoonrakerCompat> {
        let mut config = match moonraker {
            Some(text) => Config::parse(text).unwrap(),
            None => Config::default(),
        };
        config.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
        MoonrakerCompat::new(&config)
    }

    #[test]
    fn defaults_follow_moonraker_conf() {
        let c = compat(Some(""), "").unwrap();
        assert_eq!((c.enabled, c.api_key, c.source_name(LOCAL_SOURCE)), (true, Some(Role::Admin), "moonraker"));
        let c = compat(Some("[authorization]\nenable_api_key: False\n"), "").unwrap();
        assert_eq!((c.enabled, c.api_key), (true, None));
        // Without a moonraker.conf, only the section turns it on.
        assert!(!compat(None, "").unwrap().enabled);
        assert!(compat(None, "[moonraker_compat]\n").unwrap().enabled);
    }

    #[test]
    fn features_turn_off_one_by_one() {
        let c = compat(Some(""), "[moonraker_compat]\nlegacy_api_key = false\n").unwrap();
        assert_eq!((c.api_key, c.source_name(LOCAL_SOURCE)), (None, "moonraker"));
        let c =
            compat(Some(""), "[moonraker_compat]\nlegacy_api_key = \"operator\"\nuser_source_fix = false\n").unwrap();
        assert_eq!((c.api_key, c.source_name(LOCAL_SOURCE)), (Some(Role::Operator), "local"));
        assert_eq!(c.parse_source("moonraker"), "moonraker");
        let c = compat(Some(""), "[moonraker_compat]\nenabled = false\n").unwrap();
        assert_eq!((c.enabled, c.api_key, c.source_name(LOCAL_SOURCE)), (false, None, "local"));
        assert!(!c.service_alias);
        assert!(compat(Some(""), "").unwrap().service_alias);
        assert!(!compat(Some(""), "[moonraker_compat]\nservice_alias = false\n").unwrap().service_alias);
        assert_eq!(compat(Some(""), "").unwrap().source_name(LOCAL_SOURCE), "moonraker");
        // Mobileraker gets a Moonraker version, unless one is set for everyone, or none.
        let own = format!("orrery-{}", orrery_build::VERSION);
        let c = compat(Some(""), "").unwrap();
        assert_eq!(
            c.moonraker_version(Some("Mobileraker-android")),
            format!("v0.9.0-0-orrery-{}", orrery_build::VERSION)
        );
        assert_eq!((c.moonraker_version(Some("Fluidd")), c.moonraker_version(None)), (own.clone(), own.clone()));
        let c = compat(Some(""), "[moonraker_compat]\nmoonraker_version = \"v0.11.0-0-g1\"\n").unwrap();
        assert_eq!(
            (c.moonraker_version(Some("Fluidd")), c.moonraker_version(None)),
            ("v0.11.0-0-g1".into(), "v0.11.0-0-g1".into())
        );
        let c = compat(Some(""), "[moonraker_compat]\nmoonraker_version = false\n").unwrap();
        assert_eq!(c.moonraker_version(Some("Mobileraker-ios")), own);
        assert_eq!(
            compat(Some(""), "[moonraker_compat]\nenabled = false\n")
                .unwrap()
                .moonraker_version(Some("Mobileraker-ios")),
            own
        );
        for bad in [
            "[moonraker_compat]\nlegacy_api_key = \"owner\"\n",
            "[moonraker_compat]\nlegacy_api_key = true\n",
            "[moonraker_compat]\nuser_source_fix = \"moonraker\"\n",
            "[moonraker_compat]\nservice_alias = \"yes\"\n",
            "[moonraker_compat]\nmoonraker_version = \"\"\n",
            "[moonraker_compat]\nmoonraker_version = 9\n",
            "[authorization]\nenable_api_key = false\n",
        ] {
            assert!(compat(Some(""), bad).is_err(), "{bad}");
        }
    }
}
