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
            return Ok(Self { enabled, api_key: None, user_source_fix: false, service_alias: false });
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
        let user_source_fix = match table.as_ref().and_then(|t| t.get("user_source_fix")) {
            None => true,
            Some(Value::Bool(on)) => *on,
            Some(other) => bail!("[moonraker_compat] user_source_fix: {other} isn't true or false"),
        };
        let service_alias = match table.as_ref().and_then(|t| t.get("service_alias")) {
            None => true,
            Some(Value::Bool(on)) => *on,
            Some(other) => bail!("[moonraker_compat] service_alias: {other} isn't true or false"),
        };
        Ok(Self { enabled, api_key, user_source_fix, service_alias })
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
        for bad in [
            "[moonraker_compat]\nlegacy_api_key = \"owner\"\n",
            "[moonraker_compat]\nlegacy_api_key = true\n",
            "[moonraker_compat]\nuser_source_fix = \"moonraker\"\n",
            "[moonraker_compat]\nservice_alias = \"yes\"\n",
            "[authorization]\nenable_api_key = false\n",
        ] {
            assert!(compat(Some(""), bad).is_err(), "{bad}");
        }
    }
}
