//! API keys (docs/design/2026-11-20-authorization.md, "Keys").
//!
//! The config is the list of keys:
//! - `[key.<name>]` with a `role`, such as `[key.homeassistant] role = "operator"`. Its name is
//!   `<name>`.
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
                        let s = new_secret(&spec.name)?;
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
        let secret = new_secret(name)?;
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
    for section in config.section_names("key ") {
        let name = section["key ".len()..].to_owned();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            bail!("[{section}]: a key name may use letters, digits, '_' and '-'");
        }
        if name == MOONRAKER_KEY {
            bail!("[{section}]: the name moonraker is for Fluidd's API key, which orrery makes itself");
        }
        let s = config.section(&section);
        let Some(role) = s.get_opt_str("role") else {
            bail!("[{section}]: a key needs a role: \"viewer\", \"operator\", or \"admin\"");
        };
        let Some(role) = Role::parse(&role) else {
            bail!("[{section}] role: expected \"viewer\", \"operator\", or \"admin\", got {role:?}");
        };
        specs.push(Spec { name, role, agent: None });
    }
    for agent in agents.configs() {
        specs.push(Spec { name: format!("agent.{}", agent.name), role: agent.role, agent: Some(agent.name.clone()) });
    }
    if config.section("authorization").get_bool("enable_api_key", true)? {
        specs.push(Spec { name: MOONRAKER_KEY.into(), role: Role::Admin, agent: None });
    }
    Ok(specs)
}

/// A new secret: `orr_` and 32 base32 characters (160 random bits), or a UUID without dashes
/// for the `moonraker` key, as Moonraker makes it (`authorization.py:155`).
fn new_secret(name: &str) -> Result<String> {
    if name == MOONRAKER_KEY {
        return Ok(uuid::Uuid::new_v4().simple().to_string());
    }
    let mut bytes = [0u8; 20];
    let mut filled = 0;
    while filled < bytes.len() {
        filled += rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty())?;
    }
    Ok(format!("orr_{}", crate::access::base32(&bytes).to_ascii_lowercase()))
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
        let toml = "[key.homeassistant]\nrole = \"operator\"\n[agent.spoolman]\ncall_role = \"admin\"\n";
        let (config, db) = setup("", toml, dir.path());
        let (agents, _) = Agents::new(&config, None).unwrap();
        let keys = Keys::new(&config, &agents, &db).unwrap();
        let ha = keys.get("homeassistant").unwrap();
        assert!(ha.starts_with("orr_") && ha.len() == 36, "{ha}");
        assert_eq!(keys.get(MOONRAKER_KEY).unwrap().len(), 32);
        let access = keys.lookup(&ha).unwrap();
        assert_eq!((access.role, access.key.as_deref()), (Some(Role::Operator), Some("homeassistant")));
        let agent = keys.lookup(&keys.get("agent.spoolman").unwrap()).unwrap();
        assert_eq!((agent.role, agent.agent.as_deref()), (Some(Role::Viewer), Some("spoolman")));
        assert!(keys.lookup("orr_wrong").is_none());

        // A rotated key works, and the old one doesn't.
        let new = keys.rotate(&db, "homeassistant").unwrap();
        assert!(keys.lookup(&ha).is_none());
        assert!(keys.lookup(&new).is_some());
        assert!(keys.rotate(&db, "nobody").is_err());
        drop(keys);
        drop(db);

        // Secrets survive a restart; a key removed from the config is deleted.
        let moonraker = "[authorization]\nenable_api_key: False\n";
        let (config, db) = setup(moonraker, "[key.homeassistant]\nrole = \"operator\"\n", dir.path());
        let (agents, _) = Agents::new(&config, None).unwrap();
        let keys = Keys::new(&config, &agents, &db).unwrap();
        assert_eq!(keys.get("homeassistant").as_deref(), Some(new.as_str()));
        assert!(keys.get("agent.spoolman").is_none());
        assert!(keys.get(MOONRAKER_KEY).is_none());
    }

    #[test]
    fn key_tables_need_a_role() {
        let dir = tempfile::tempdir().unwrap();
        for toml in ["[key.ha]\n", "[key.ha]\nrole = \"agent\"\n", "[key.moonraker]\nrole = \"admin\"\n"] {
            let (config, db) = setup("", toml, dir.path());
            let (agents, _) = Agents::new(&config, None).unwrap();
            assert!(Keys::new(&config, &agents, &db).is_err(), "{toml}");
        }
    }
}
