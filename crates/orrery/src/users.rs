//! Users and their logins (docs/design/2026-11-21-logins.md).
//!
//! - Roles come from the config: `[authorization.users]` lists the users for each role, such
//!   as `admin = ["sami"]`. A user who isn't listed gets `default_role`, or nothing. Without the
//!   list, local users are admin, as every Moonraker user is.
//! - Local users (source `local`, which Moonraker's replies call `moonraker`; see
//!   [`crate::compat`]) live in a redb table of their own, with their password
//!   hash and signing key. New passwords are hashed with Argon2id; Moonraker's PBKDF2 hashes,
//!   from an import, are checked and replaced at the next login.
//! - Tokens are Moonraker's JWTs, signed with EdDSA, with a signing key per user.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::ApiError;
use crate::auth::Role;
use crate::config::Config;
use crate::database::Database;
use crate::keys::{constant_time_eq, hex, random_bytes};

/// `user name -> user`, as JSON.
pub(crate) const USERS: TableDefinition<&str, &str> = TableDefinition::new("users");

pub use crate::compat::LOCAL_SOURCE;

/// Names Moonraker keeps for itself.
const RESERVED: &[&str] = &["_API_KEY_USER_", "_TRUSTED_USER_"];

/// How long an access token lasts, as in Moonraker.
const ACCESS_TOKEN_SECS: u64 = 3600;

/// Argon2id's parameters, as OWASP recommends: 19 MiB, 2 passes, 1 lane.
const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
const ARGON2_PASSES: u32 = 2;
const ARGON2_LANES: u32 = 1;

/// A stored user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub username: String,
    pub source: String,
    pub created_on: f64,
    /// `argon2id$m=…,t=…,p=…$<salt>$<hash>` or `pbkdf2-sha256$<rounds>$<salt>$<hash>`, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// The signing key's 32-byte seed, in hex, as Moonraker's `jwt_secret`. `None`: logged out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_secret: Option<String>,
    /// The signing key's ID, the JWT's `kid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwk_id: Option<String>,
}

/// The tokens a login gives.
pub struct Tokens {
    pub token: String,
    pub refresh_token: String,
}

/// What a token is for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenType {
    Access,
    Refresh,
}

impl TokenType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Refresh => "refresh",
        }
    }
}

pub struct Users {
    /// `[authorization.users]`, by name. `None`: the config has no list.
    roles: Option<HashMap<String, Role>>,
    default_role: Option<Role>,
    pub local_logins: bool,
    /// The JWTs' `iss`: `http://<hostname>:<port>`, as in Moonraker.
    issuer: String,
    /// How long a refresh token lasts: `login_timeout` days.
    refresh_secs: u64,
    users: RwLock<HashMap<String, User>>,
}

impl Users {
    /// Reads the config and the stored users. `is_key` tells whether a name is a key's.
    pub fn new(config: &Config, is_key: impl Fn(&str) -> bool, db: &Database) -> Result<Self> {
        let section = config.section("authorization");
        let roles = match config.section_json("authorization users") {
            Some(table) => Some(role_lists(table, &is_key)?),
            None => None,
        };
        let default_role = match section.has("default_role") {
            true => {
                let text = section.get_str("default_role", "");
                Some(Role::parse(&text).with_context(|| {
                    format!("[authorization] default_role: {text:?} isn't a role. Use viewer, operator, or admin")
                })?)
            }
            false => None,
        };
        // orrery's own option: read only when it's set, so server.config shows only Moonraker's.
        let local_logins = !section.has("local_logins") || section.get_bool("local_logins", true)?;
        let days = section.get_int("login_timeout", 90)?;
        if days < 1 {
            bail!("[authorization] login_timeout: {days} isn't a number of days. Use 1 or more, such as 90");
        }
        let hostname = rustix::system::uname().nodename().to_string_lossy().into_owned();
        let port = config.section("server").get_int("port", 7125)?;
        let mut users = HashMap::new();
        let tx = db.raw().begin_write()?;
        {
            let table = tx.open_table(USERS)?;
            for entry in table.iter()? {
                let (name, value) = entry?;
                let mut user: User = serde_json::from_str(value.value())
                    .with_context(|| format!("the stored user {} can't be read", name.value()))?;
                // Users stored before 2026-11-21 have Moonraker's name for the source.
                if user.source == "moonraker" {
                    user.source = LOCAL_SOURCE.to_owned();
                }
                users.insert(name.value().to_owned(), user);
            }
        }
        tx.commit()?;
        Ok(Self {
            roles,
            default_role,
            local_logins,
            issuer: format!("http://{hostname}:{port}"),
            refresh_secs: u64::try_from(days).unwrap_or(90) * 86400,
            users: RwLock::new(users),
        })
    }

    /// The startup log lines.
    pub fn log_settings(&self) {
        let count = self.users.read().expect("users lock").len();
        match &self.roles {
            None if count > 0 => tracing::warn!(
                target: "orrery::auth",
                "Users: orrery.toml has no [authorization.users], so all {count} local users are admin, as in Moonraker. List them by role, such as admin = [\"sami\"], to give them less."
            ),
            None => tracing::info!(target: "orrery::auth", "Users: none"),
            Some(roles) => {
                let mut by_role: Vec<(&String, &Role)> = roles.iter().collect();
                by_role.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                let list: Vec<String> = by_role.iter().map(|(n, r)| format!("{n} ({r})")).collect();
                let others = match self.default_role {
                    Some(r) => format!("others: {r}"),
                    None => "others: no role".to_owned(),
                };
                tracing::info!(target: "orrery::auth", "Users: {}; {others}", list.join(", "));
            }
        }
        if self.local_logins {
            tracing::info!(target: "orrery::auth", "Logins: local passwords ({count} users)");
        } else {
            tracing::info!(target: "orrery::auth", "Logins: local passwords are off (local_logins = false)");
        }
    }

    /// A user's role, from the config.
    pub fn role_of(&self, name: &str) -> Result<Role, ApiError> {
        let Some(roles) = &self.roles else { return Ok(Role::Admin) };
        roles.get(name).copied().or(self.default_role).ok_or_else(|| {
            ApiError::new(
                403,
                format!("orrery.toml doesn't give the user {name} a role. An admin can add them under [authorization.users]."),
            )
        })
    }

    pub fn get(&self, name: &str) -> Option<User> {
        self.users.read().expect("users lock").get(name).cloned()
    }

    /// The stored users, for `access.users.list`.
    pub fn list(&self) -> Vec<User> {
        let mut users: Vec<User> = self.users.read().expect("users lock").values().cloned().collect();
        users.sort_by(|a, b| a.created_on.total_cmp(&b.created_on));
        users
    }

    /// Checks a name for a new user.
    pub fn check_new_name(&self, name: &str, is_key: impl Fn(&str) -> bool) -> Result<(), ApiError> {
        check_name(name).map_err(|e| ApiError::bad_request(e.to_string()))?;
        if is_key(name) {
            return Err(ApiError::bad_request(format!(
                "{name} is the name of a key in orrery.toml's [authorization.keys], so a user can't have it"
            )));
        }
        if self.users.read().expect("users lock").contains_key(name) {
            return Err(ApiError::bad_request(format!("The user {name} already exists")));
        }
        Ok(())
    }

    /// Stores a user, replacing one of the same name.
    pub fn store(&self, db: &Database, user: User) -> Result<()> {
        let tx = db.raw().begin_write()?;
        tx.open_table(USERS)?.insert(user.username.as_str(), serde_json::to_string(&user)?.as_str())?;
        tx.commit()?;
        self.users.write().expect("users lock").insert(user.username.clone(), user);
        Ok(())
    }

    pub fn remove(&self, db: &Database, name: &str) -> Result<bool> {
        let tx = db.raw().begin_write()?;
        let found = tx.open_table(USERS)?.remove(name)?.is_some();
        tx.commit()?;
        self.users.write().expect("users lock").remove(name);
        Ok(found)
    }

    /// A new local user, with a password hash from [`hash_password`].
    pub fn new_local(name: &str, hash: String) -> User {
        User {
            username: name.to_owned(),
            source: LOCAL_SOURCE.to_owned(),
            created_on: now_secs(),
            password: Some(hash),
            jwt_secret: None,
            jwk_id: None,
        }
    }

    /// Gives a user a signing key if they have none, and makes their tokens.
    pub fn log_in(&self, db: &Database, name: &str) -> Result<Tokens> {
        let mut user = self.get(name).with_context(|| format!("no user {name}"))?;
        if user.jwt_secret.is_none() || user.jwk_id.is_none() {
            user.jwt_secret = Some(hex(&random_bytes::<32>()?));
            user.jwk_id = Some(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?));
            self.store(db, user.clone())?;
        }
        Ok(Tokens {
            token: self.token(&user, TokenType::Access, ACCESS_TOKEN_SECS)?,
            refresh_token: self.token(&user, TokenType::Refresh, self.refresh_secs)?,
        })
    }

    /// A new access token, for `access.refresh_jwt`.
    pub fn access_token(&self, name: &str) -> Result<String> {
        let user = self.get(name).with_context(|| format!("no user {name}"))?;
        self.token(&user, TokenType::Access, ACCESS_TOKEN_SECS)
    }

    /// Forgets a user's signing key, so every token they have stops working: a logout.
    pub fn forget_key(&self, db: &Database, name: &str) -> Result<()> {
        if let Some(mut user) = self.get(name) {
            user.jwt_secret = None;
            user.jwk_id = None;
            self.store(db, user)?;
        }
        Ok(())
    }

    fn token(&self, user: &User, kind: TokenType, lasts: u64) -> Result<String> {
        let (Some(seed), Some(kid)) = (&user.jwt_secret, &user.jwk_id) else {
            bail!("the user {} is logged out", user.username);
        };
        let key = signing_key(seed)?;
        let now = now_secs() as u64;
        let header = json!({"kid": kid, "alg": "EdDSA", "typ": "JWT"});
        let payload = json!({
            "iss": self.issuer,
            "aud": "Moonraker",
            "iat": now,
            "exp": now + lasts,
            "username": user.username,
            "token_type": kind.as_str(),
        });
        let message =
            format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(payload.to_string()));
        let signature = key.sign(message.as_bytes());
        Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
    }

    /// The user a token is for. `check_exp` is false where Moonraker accepts an expired token:
    /// on endpoints that don't need one.
    pub fn check_token(&self, token: &str, kind: TokenType, check_exp: bool) -> Result<String, ApiError> {
        let invalid = |why: &str| ApiError::new(401, format!("Invalid token: {why}"));
        let (message, signature) = token.rsplit_once('.').ok_or_else(|| invalid("it isn't a JWT"))?;
        let (header, payload) = message.split_once('.').ok_or_else(|| invalid("it isn't a JWT"))?;
        let header: Value = decode_part(header).ok_or_else(|| invalid("its header can't be read"))?;
        if header["typ"] != "JWT" || header["alg"] != "EdDSA" {
            return Err(invalid("it isn't one of orrery's"));
        }
        let payload: Value = decode_part(payload).ok_or_else(|| invalid("its payload can't be read"))?;
        let name = payload["username"].as_str().ok_or_else(|| invalid("it names no user"))?;
        let user = self.get(name).ok_or_else(|| invalid("its user doesn't exist"))?;
        let (Some(seed), Some(kid)) = (&user.jwt_secret, &user.jwk_id) else {
            return Err(invalid("its user has logged out"));
        };
        if header["kid"].as_str() != Some(kid.as_str()) {
            return Err(invalid("its user has logged out since"));
        }
        let key = signing_key(seed).map_err(|_| invalid("its user's signing key can't be read"))?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature.trim_end_matches('='))
            .ok()
            .and_then(|s| ed25519_dalek::Signature::from_slice(&s).ok())
            .ok_or_else(|| invalid("its signature can't be read"))?;
        verifying_key(&key).verify(message.as_bytes(), &signature).map_err(|_| invalid("its signature is wrong"))?;
        if payload["token_type"] != kind.as_str() {
            return Err(invalid(&format!("it isn't an {} token", kind.as_str())));
        }
        if payload["iss"] != self.issuer.as_str() || payload["aud"] != "Moonraker" {
            return Err(invalid("it's for another server"));
        }
        if check_exp && payload["exp"].as_f64().is_none_or(|exp| exp < now_secs()) {
            return Err(ApiError::new(401, "The token has expired"));
        }
        Ok(name.to_owned())
    }
}

/// Reads `[authorization.users]`: the users for each role.
fn role_lists(table: serde_json::Map<String, Value>, is_key: impl Fn(&str) -> bool) -> Result<HashMap<String, Role>> {
    let mut roles: HashMap<String, Role> = HashMap::new();
    for (role_name, list) in table {
        let Some(role) = Role::parse(&role_name) else {
            bail!(
                "[authorization.users]: {role_name} isn't a role. Use viewer, operator, or admin, such as \
                 admin = [\"sami\"]"
            );
        };
        let names: Vec<&Value> = match &list {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for name in names {
            let Some(name) = name.as_str() else {
                bail!("[authorization.users] {role_name}: {name} isn't a user name in quotes");
            };
            if name.starts_with("group:") {
                bail!("[authorization.users] {role_name}: {name:?}: groups aren't supported yet");
            }
            check_name(name).with_context(|| format!("[authorization.users] {role_name}"))?;
            if is_key(name) {
                bail!(
                    "[authorization.users] {role_name}: {name} is also a key in [authorization.keys]. The user list \
                     shows keys too, so give the user another name."
                );
            }
            if let Some(other) = roles.get(name) {
                bail!(
                    "[authorization.users]: the user {name} is listed under both {other} and {role_name}. Keep one of them."
                );
            }
            roles.insert(name.to_owned(), role);
        }
    }
    Ok(roles)
}

/// A user name: not empty, no control characters or spaces at the ends, and none of
/// Moonraker's own names.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("{name:?}: a user name has 1 to 64 characters");
    }
    if name.chars().any(char::is_control) || name.trim() != name {
        bail!("{name:?}: a user name can't have control characters, or spaces at either end");
    }
    if RESERVED.contains(&name) || name.starts_with("group:") || name.starts_with("agent.") {
        bail!("{name:?}: that name is reserved");
    }
    Ok(())
}

/// Hashes a new password with Argon2id. It takes a fraction of a second, so call it off the
/// async threads.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = random_bytes::<16>()?;
    let hash = argon2id(password, &salt, ARGON2_MEMORY_KIB, ARGON2_PASSES, ARGON2_LANES)?;
    Ok(format!("argon2id$m={ARGON2_MEMORY_KIB},t={ARGON2_PASSES},p={ARGON2_LANES}${}${}", hex(&salt), hex(&hash)))
}

/// Whether a password matches a stored hash, and whether the hash should be replaced with a
/// new one: Moonraker's PBKDF2. Slow, like [`hash_password`].
pub fn check_password(password: &str, stored: &str) -> (bool, bool) {
    let parts: Vec<&str> = stored.split('$').collect();
    let unhex = |s: &str| -> Option<Vec<u8>> {
        (s.is_ascii() && s.len().is_multiple_of(2))
            .then(|| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect())
            .flatten()
    };
    match parts.as_slice() {
        ["argon2id", params, salt, hash] => {
            let mut m = 0;
            let mut t = 0;
            let mut p = 0;
            for kv in params.split(',') {
                match kv.split_once('=') {
                    Some(("m", v)) => m = v.parse().unwrap_or(0),
                    Some(("t", v)) => t = v.parse().unwrap_or(0),
                    Some(("p", v)) => p = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
            let (Some(salt), Some(hash)) = (unhex(salt), unhex(hash)) else { return (false, false) };
            let ok = argon2id(password, &salt, m, t, p).is_ok_and(|h| constant_time_eq(&h, &hash));
            let outdated = (m, t, p) != (ARGON2_MEMORY_KIB, ARGON2_PASSES, ARGON2_LANES);
            (ok, ok && outdated)
        }
        ["pbkdf2-sha256", rounds, salt, hash] => {
            let (Ok(rounds @ 1..), Some(salt), Some(hash)) = (rounds.parse::<u32>(), unhex(salt), unhex(hash)) else {
                return (false, false);
            };
            let mut out = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, rounds, &mut out);
            let ok = constant_time_eq(&out, &hash);
            (ok, ok)
        }
        _ => (false, false),
    }
}

fn argon2id(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32]> {
    let params = argon2::Params::new(m, t, p, Some(32)).map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    argon.hash_password_into(password.as_bytes(), salt, &mut out).map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(out)
}

fn signing_key(seed_hex: &str) -> Result<SigningKey> {
    if !seed_hex.is_ascii() {
        bail!("the signing key isn't hex");
    }
    let bytes: Vec<u8> = (0..seed_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(seed_hex.get(i..i + 2).unwrap_or("x"), 16))
        .collect::<Result<_, _>>()
        .context("the signing key isn't hex")?;
    let seed: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("the signing key isn't 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn verifying_key(key: &SigningKey) -> VerifyingKey {
    key.verifying_key()
}

/// A JWT part: base64url JSON, with or without padding.
fn decode_part(part: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Paths;

    fn setup(moonraker: &str, toml: &str, dir: &std::path::Path) -> (Users, Database) {
        let mut config = Config::parse(moonraker).unwrap();
        config.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
        let paths = Paths { data: dir.to_path_buf(), config_file: dir.join("moonraker.conf"), log_file: None };
        let db = Database::open(&config, &paths).unwrap();
        let users = Users::new(&config, |n| n == "homeassistant", &db).unwrap();
        (users, db)
    }

    #[test]
    fn passwords() {
        let hash = hash_password("hunter2").unwrap();
        assert!(hash.starts_with("argon2id$m=19456,t=2,p=1$"), "{hash}");
        assert_eq!(check_password("hunter2", &hash), (true, false));
        assert_eq!(check_password("hunter3", &hash), (false, false));
        // Moonraker's hash of "test" with the salt 00..1f (hashlib.pbkdf2_hmac, 100000 rounds).
        let salt = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let mut out = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"test", &(0..32).collect::<Vec<u8>>(), 100_000, &mut out);
        let stored = format!("pbkdf2-sha256$100000${salt}${}", hex(&out));
        assert_eq!(check_password("test", &stored), (true, true));
        assert_eq!(check_password("tset", &stored), (false, false));
        assert_eq!(check_password("test", "garbage"), (false, false));
    }

    #[test]
    fn tokens() {
        let dir = tempfile::tempdir().unwrap();
        let (users, db) = setup("", "", dir.path());
        users.store(&db, Users::new_local("sami", hash_password("pw").unwrap())).unwrap();
        let tokens = users.log_in(&db, "sami").unwrap();
        assert_eq!(users.check_token(&tokens.token, TokenType::Access, true).unwrap(), "sami");
        assert!(users.check_token(&tokens.token, TokenType::Refresh, true).is_err());
        assert_eq!(users.check_token(&tokens.refresh_token, TokenType::Refresh, true).unwrap(), "sami");
        // A changed byte in the signature or payload is refused.
        let mut bad = tokens.token.clone();
        let last = bad.pop().unwrap();
        bad.push(if last == 'A' { 'B' } else { 'A' });
        assert!(users.check_token(&bad, TokenType::Access, true).is_err());
        let parts: Vec<&str> = tokens.token.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(
            json!({"iss": users.issuer, "aud": "Moonraker", "iat": 0, "exp": 9e12, "username": "sami",
                   "token_type": "access"})
            .to_string(),
        );
        assert!(users.check_token(&format!("{}.{forged}.{}", parts[0], parts[2]), TokenType::Access, true).is_err());
        // Logging out ends every token.
        users.forget_key(&db, "sami").unwrap();
        assert!(users.check_token(&tokens.token, TokenType::Access, true).is_err());
        assert!(users.check_token(&tokens.refresh_token, TokenType::Refresh, true).is_err());
        // Users survive a restart.
        drop(users);
        drop(db);
        let (users, _db) = setup("", "", dir.path());
        assert_eq!(users.get("sami").unwrap().source, "local");
    }

    #[test]
    fn roles() {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let (users, _db) = setup("", "", dirs[0].path());
        assert_eq!(users.role_of("anyone").unwrap(), Role::Admin);
        let (users, _db) =
            setup("", "[authorization.users]\nadmin = [\"sami\"]\noperator = \"guest\"\n", dirs[1].path());
        assert_eq!(users.role_of("sami").unwrap(), Role::Admin);
        assert_eq!(users.role_of("guest").unwrap(), Role::Operator);
        let refused = users.role_of("kid").unwrap_err();
        assert_eq!(refused.status, 403);
        assert!(!refused.message.contains("sami"), "{}", refused.message);
        let (users, _db) = setup(
            "",
            "[authorization]\ndefault_role = \"viewer\"\n[authorization.users]\nadmin = \"sami\"\n",
            dirs[2].path(),
        );
        assert_eq!(users.role_of("kid").unwrap(), Role::Viewer);
    }

    #[test]
    fn user_lists_are_checked() {
        for toml in [
            "[authorization.users]\nowner = [\"sami\"]\n",
            "[authorization.users]\nadmin = [\"sami\"]\nviewer = [\"sami\"]\n",
            "[authorization.users]\nadmin = [\"homeassistant\"]\n",
            "[authorization.users]\nadmin = [\"group:admins\"]\n",
            "[authorization.users]\nadmin = [\"_API_KEY_USER_\"]\n",
            "[authorization]\ndefault_role = \"owner\"\n",
            "[authorization]\nlogin_timeout = 0\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::parse("").unwrap();
            config.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
            let paths = Paths {
                data: dir.path().to_path_buf(),
                config_file: dir.path().join("moonraker.conf"),
                log_file: None,
            };
            let db = Database::open(&config, &paths).unwrap();
            assert!(Users::new(&config, |n| n == "homeassistant", &db).is_err(), "{toml}");
        }
    }
}
