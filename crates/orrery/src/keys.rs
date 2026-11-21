//! API keys (docs/design/2026-11-20-authorization.md, "Keys").
//!
//! The config is the list of keys:
//! - `[authorization.keys]`, which lists the keys for each role, such as
//!   `operator = ["homeassistant"]`. Each key's name is the one listed.
//! - Each `[agent.<name>]`, with the agent's rights. Its name is `agent.<name>`.
//! - `moonraker`: Fluidd's API key, with the admin role, unless `enable_api_key` is off.
//!
//! orrery makes each key's secret on startup, and keeps the secrets in a redb table of their
//! own, which `server.database.*` can't reach. A secret whose key is gone from the config is
//! deleted. An admin can read a key again, or rotate it; to add or remove a key, edit the
//! config.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{Result, bail};
use redb::{ReadableTable, TableDefinition};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agents::Agents;
use crate::auth::{Access, Role};
use crate::config::Config;
use crate::database::Database;

/// `key name -> secret`.
pub(crate) const CREDENTIALS: TableDefinition<&str, &str> = TableDefinition::new("credentials");

/// The name of Fluidd's API key, Moonraker's one key.
pub const MOONRAKER_KEY: &str = "moonraker";

/// A key the config declares.
#[derive(Debug, Clone, PartialEq)]
struct Spec {
    name: String,
    role: Role,
    /// An agent's key speaks for the agent.
    agent: Option<String>,
}

/// A secret, with its digest for comparing.
struct Secret {
    secret: String,
    digest: [u8; 32],
}

impl Secret {
    fn new(secret: String) -> Self {
        let digest = Sha256::digest(secret.as_bytes()).into();
        Self { secret, digest }
    }
}

pub struct Keys {
    specs: Vec<Spec>,
    secrets: RwLock<HashMap<String, Secret>>,
}

impl Keys {
    /// Reads the declared keys, makes the missing secrets, and deletes the secrets of keys that
    /// are gone.
    pub fn new(config: &Config, agents: &Agents, db: &Database) -> Result<Self> {
        let specs = specs(config, agents)?;
        let tx = db.raw().begin_write()?;
        let mut secrets = HashMap::new();
        {
            let mut table = tx.open_table(CREDENTIALS)?;
            let mut gone = Vec::new();
            for entry in table.iter()? {
                let (name, _) = entry?;
                let name = name.value().to_owned();
                if !specs.iter().any(|s| s.name == name) {
                    gone.push(name);
                }
            }
            for name in gone {
                table.remove(name.as_str())?;
                tracing::info!(target: "orrery::auth", "Deleted the key {name}: the config doesn't list it any more");
            }
            for spec in &specs {
                let existing = table.get(spec.name.as_str())?.map(|v| v.value().to_owned());
                let secret = match existing {
                    Some(s) => s,
                    None => {
                        let s = new_secret()?;
                        table.insert(spec.name.as_str(), s.as_str())?;
                        tracing::info!(target: "orrery::auth", "Made the key {} ({})", spec.name, spec.role);
                        s
                    }
                };
                secrets.insert(spec.name.clone(), Secret::new(secret));
            }
        }
        tx.commit()?;
        Ok(Self { specs, secrets: RwLock::new(secrets) })
    }

    /// The startup log line: every key, with its role.
    pub fn log_settings(&self) {
        if self.specs.is_empty() {
            tracing::info!(target: "orrery::auth", "Authorization: no keys");
            return;
        }
        let list: Vec<String> = self.specs.iter().map(|s| format!("{} ({})", s.name, describe_role(s))).collect();
        tracing::info!(target: "orrery::auth", "Authorization: keys: {}", list.join(", "));
    }

    /// The rights a secret gives, or `None` for a secret that isn't a key's.
    pub fn lookup(&self, presented: &str) -> Option<Access> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let secrets = self.secrets.read().expect("secrets lock");
        // Every key is compared, in constant time, so the time taken doesn't tell which matched.
        let mut found = None;
        for (name, secret) in secrets.iter() {
            if constant_time_eq(&secret.digest, &digest) {
                found = Some(name.clone());
            }
        }
        let spec = self.specs.iter().find(|s| Some(&s.name) == found.as_ref())?;
        Some(Access {
            role: Some(spec.role),
            agent: spec.agent.clone(),
            trusted: false,
            key: Some(spec.name.clone()),
            source: format!("key {}", spec.name),
            client: String::new(),
        })
    }

    /// Whether the config declares a key of this name.
    pub fn exists(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }

    /// A key's secret.
    pub fn get(&self, name: &str) -> Option<String> {
        self.secrets.read().expect("secrets lock").get(name).map(|s| s.secret.clone())
    }

    /// Gives a key a new secret, and returns it. The old secret stops working at once.
    pub fn rotate(&self, db: &Database, name: &str) -> Result<String> {
        if !self.exists(name) {
            bail!("No key named {name}");
        }
        let secret = new_secret()?;
        let tx = db.raw().begin_write()?;
        tx.open_table(CREDENTIALS)?.insert(name, secret.as_str())?;
        tx.commit()?;
        self.secrets.write().expect("secrets lock").insert(name.to_owned(), Secret::new(secret.clone()));
        Ok(secret)
    }
}

fn describe_role(spec: &Spec) -> String {
    match &spec.agent {
        Some(agent) if spec.role == Role::Viewer => format!("agent {agent}"),
        Some(agent) => format!("agent {agent}, {}", spec.role),
        None => spec.role.to_string(),
    }
}

/// The keys the config declares.
fn specs(config: &Config, agents: &Agents) -> Result<Vec<Spec>> {
    let mut specs = Vec::new();
    if let Some(section) = config.section_names("key ").first() {
        bail!(
            "[{section}]: keys are now listed by role in [authorization.keys], such as \
             operator = [\"homeassistant\"]. Move the key there."
        );
    }
    // The keys for each role, such as operator = ["homeassistant", "mobileraker"].
    for (role_name, list) in config.section_json("authorization keys").unwrap_or_default() {
        let Some(role) = Role::parse(&role_name) else {
            bail!(
                "[authorization.keys]: {role_name} isn't a role. Use viewer, operator, or admin, such as \
                 operator = [\"homeassistant\"]"
            );
        };
        let names: Vec<&Value> = match &list {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for name in names {
            let Some(name) = name.as_str() else {
                bail!("[authorization.keys] {role_name}: {name} isn't a key name in quotes");
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                bail!("[authorization.keys] {role_name}: {name:?}: a key name may use letters, digits, '_' and '-'");
            }
            if name == MOONRAKER_KEY {
                bail!(
                    "[authorization.keys] {role_name}: the name moonraker is for Fluidd's API key, which orrery \
                     makes itself"
                );
            }
            if let Some(other) = specs.iter().find(|s: &&Spec| s.name == name) {
                bail!(
                    "[authorization.keys]: the key {name} is listed under both {} and {role_name}. Keep one of them.",
                    other.role
                );
            }
            specs.push(Spec { name: name.to_owned(), role, agent: None });
        }
    }
    for agent in agents.configs() {
        specs.push(Spec { name: format!("agent.{}", agent.name), role: agent.role, agent: Some(agent.name.clone()) });
    }
    if config.section("authorization").get_bool("enable_api_key", true)? {
        specs.push(Spec { name: MOONRAKER_KEY.into(), role: Role::Admin, agent: None });
    }
    Ok(specs)
}

/// A new secret, in the format of Moonraker's API key: 32 lowercase hex characters, here 128
/// random bits. Clients check that format: Home Assistant's integration accepts only 32 letters
/// and digits (`config_flow.py:106`). No prefix, so a key doesn't say what made it.
fn new_secret() -> Result<String> {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        filled += rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty())?;
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Paths;

    fn setup(moonraker: &str, toml: &str, dir: &std::path::Path) -> (Config, Database) {
        let mut config = Config::parse(moonraker).unwrap();
        config.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
        let paths = Paths { data: dir.to_path_buf(), config_file: dir.join("moonraker.conf"), log_file: None };
        let db = Database::open(&config, &paths).unwrap();
        (config, db)
    }

    #[test]
    fn keys_follow_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let toml = "[authorization.keys]\noperator = [\"homeassistant\"]\n[agent.spoolman]\ncall_role = \"admin\"\n";
        let (config, db) = setup("", toml, dir.path());
        let (agents, _) = Agents::new(&config, None).unwrap();
        let keys = Keys::new(&config, &agents, &db).unwrap();
        let ha = keys.get("homeassistant").unwrap();
        assert!(ha.len() == 32 && ha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{ha}");
        assert_eq!(keys.get(MOONRAKER_KEY).unwrap().len(), 32);
        let access = keys.lookup(&ha).unwrap();
        assert_eq!((access.role, access.key.as_deref()), (Some(Role::Operator), Some("homeassistant")));
        let agent = keys.lookup(&keys.get("agent.spoolman").unwrap()).unwrap();
        assert_eq!((agent.role, agent.agent.as_deref()), (Some(Role::Viewer), Some("spoolman")));
        assert!(keys.lookup("0123456789abcdef0123456789abcdef").is_none());

        // A rotated key works, and the old one doesn't.
        let new = keys.rotate(&db, "homeassistant").unwrap();
        assert!(keys.lookup(&ha).is_none());
        assert!(keys.lookup(&new).is_some());
        assert!(keys.rotate(&db, "nobody").is_err());
        drop(keys);
        drop(db);

        // Secrets survive a restart; a key removed from the config is deleted.
        let moonraker = "[authorization]\nenable_api_key: False\n";
        let (config, db) = setup(moonraker, "[authorization.keys]\noperator = \"homeassistant\"\n", dir.path());
        let (agents, _) = Agents::new(&config, None).unwrap();
        let keys = Keys::new(&config, &agents, &db).unwrap();
        assert_eq!(keys.get("homeassistant").as_deref(), Some(new.as_str()));
        assert!(keys.get("agent.spoolman").is_none());
        assert!(keys.get(MOONRAKER_KEY).is_none());
    }

    #[test]
    fn key_lists_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        for toml in [
            "[key.ha]\nrole = \"operator\"\n",
            "[authorization.keys]\nagent = [\"ha\"]\n",
            "[authorization.keys]\nadmin = [\"moonraker\"]\n",
            "[authorization.keys]\nadmin = [\"ha\"]\nviewer = [\"ha\"]\n",
            "[authorization.keys]\nviewer = [\"agent.spoolman\"]\n",
        ] {
            let (config, db) = setup("", toml, dir.path());
            let (agents, _) = Agents::new(&config, None).unwrap();
            assert!(Keys::new(&config, &agents, &db).is_err(), "{toml}");
        }
    }
}
