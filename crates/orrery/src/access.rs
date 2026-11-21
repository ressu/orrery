//! The parts of Moonraker's `authorization.py` that frontends need when nobody logs in: the
//! `access.*` endpoints, and CORS. Who may do what is in [`crate::auth`]. orrery has no user
//! logins yet, so a client is the `_TRUSTED_USER_`, or the `_API_KEY_USER_` with a key.
//! Oneshot tokens are handed out so frontends can build download links, and they aren't
//! checked. CORS follows `[authorization] cors_domains` and the networks that have a role.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::json;

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::auth::Network;
use crate::config::Config;
use crate::server::App;

pub const TRUSTED_USER: &str = "_TRUSTED_USER_";
pub const API_KEY_USER: &str = "_API_KEY_USER_";

/// When the trusted user was first seen, as Moonraker records it.
static TRUSTED_SINCE: OnceLock<f64> = OnceLock::new();

/// Which browser origins may call the API, after Moonraker's `check_cors`.
pub struct Cors {
    domains: Vec<Regex>,
    /// Trusted networks: an origin whose host is a trusted address is allowed too.
    trusted: Vec<Network>,
}

impl Cors {
    /// Reads `[authorization] cors_domains`. Returns the warnings Moonraker gives for domains
    /// it skips.
    pub fn new(config: &Config, trusted: Vec<Network>) -> (Self, Vec<String>) {
        let section = config.section("authorization");
        let mut warnings = Vec::new();
        let mut domains = Vec::new();
        let unsafe_wildcard = Regex::new(r"^.+\.[^:]*\*").expect("valid pattern");
        for domain in section.get_list("cors_domains") {
            if unsafe_wildcard.is_match(&domain) {
                warnings.push(format!(
                    "[authorization]: Unsafe domain '{domain}' in option 'cors_domains'. Wildcards are not \
                     permitted in the top level domain."
                ));
                continue;
            }
            if domain.ends_with('/') {
                warnings.push(format!(
                    "[authorization]: Invalid domain '{domain}' in option 'cors_domains'.  Domain's cannot \
                     contain a trailing slash."
                ));
                continue;
            }
            let pattern = domain.replace('.', "\\.").replace('*', ".*");
            match Regex::new(&format!("^(?:{pattern})$")) {
                Ok(re) => domains.push(re),
                Err(e) => {
                    warnings.push(format!("[authorization]: Invalid domain '{domain}' in option 'cors_domains': {e}"))
                }
            }
        }
        (Self { domains, trusted }, warnings)
    }

    /// Whether a request from `origin` gets CORS headers.
    pub fn allows(&self, origin: &str) -> bool {
        if self.domains.is_empty() {
            return false;
        }
        if self.domains.iter().any(|re| re.is_match(origin)) {
            return true;
        }
        // An origin whose host is a trusted IP is allowed too.
        let host = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://"));
        let host = host.map(|h| h.split(['/', ':']).next().unwrap_or_default());
        host.and_then(|h| h.parse::<IpAddr>().ok()).is_some_and(|ip| self.trusted.iter().any(|n| n.contains(ip)))
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register(
        "/access/user",
        &[RequestType::Get, RequestType::Post, RequestType::Delete],
        Transports::All,
        handler(user),
    );
    r.register("/access/users/list", &[RequestType::Get], Transports::All, handler(users_list));
    r.register("/access/oneshot_token", &[RequestType::Get], Transports::All, handler(oneshot_token));
    r.register("/access/info", &[RequestType::Get], Transports::All, handler(info));
    r.register("/access/api_key", &[RequestType::Get, RequestType::Post], Transports::All, handler(api_key));
    r.register("/orrery/access/key/get", &[RequestType::Get], Transports::All, handler(key_get));
    r.register("/orrery/access/key/rotate", &[RequestType::Post], Transports::All, handler(key_rotate));
}

/// Fluidd's API key: `access.get_api_key` reads it and `access.post_api_key` makes a new one,
/// as in Moonraker. It's the `moonraker` key.
async fn api_key(app: Arc<App>, req: Request) -> ApiResult {
    let name = crate::keys::MOONRAKER_KEY;
    if !app.keys.exists(name) {
        return Err(ApiError::bad_request("The API key is off: [authorization] enable_api_key is False"));
    }
    if req.kind == RequestType::Post {
        return rotate(&app, &req, name);
    }
    Ok(json!(app.keys.get(name)))
}

/// `orrery.access.key.get {name}`: a key's secret.
async fn key_get(app: Arc<App>, req: Request) -> ApiResult {
    let name = req.args.str("name")?;
    app.keys.get(&name).map(|s| json!({"name": name, "key": s})).ok_or_else(|| no_key(&name))
}

/// `orrery.access.key.rotate {name}`: gives a key a new secret, and returns it.
async fn key_rotate(app: Arc<App>, req: Request) -> ApiResult {
    let name = req.args.str("name")?;
    if !app.keys.exists(&name) {
        return Err(no_key(&name));
    }
    rotate(&app, &req, &name).map(|key| json!({"name": name, "key": key}))
}

fn rotate(app: &App, req: &Request, name: &str) -> ApiResult {
    let secret = app.keys.rotate(&app.database, name)?;
    tracing::info!(target: "orrery::auth", "Rotated the key {name}, as {} asked", req.access.describe());
    app.revoke_key(name);
    Ok(json!(secret))
}

fn no_key(name: &str) -> ApiError {
    ApiError::not_found(format!("No key named {name}: keys are declared in orrery.toml, as [key.<name>]"))
}

async fn user(_app: Arc<App>, req: Request) -> ApiResult {
    if req.kind != RequestType::Get {
        return Err(ApiError::bad_request("User accounts aren't available: orrery has no user logins yet"));
    }
    let since =
        *TRUSTED_SINCE.get_or_init(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64());
    // As in Moonraker: a key's client is the API key user, everyone else the trusted user.
    let name = if req.access.key.is_some() { API_KEY_USER } else { TRUSTED_USER };
    Ok(json!({"username": name, "source": "moonraker", "created_on": since}))
}

async fn users_list(_app: Arc<App>, _req: Request) -> ApiResult {
    Ok(json!({"users": []}))
}

async fn oneshot_token(_app: Arc<App>, _req: Request) -> ApiResult {
    // Moonraker: base32 of 20 random bytes.
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..4]);
    Ok(json!(base32(&bytes)))
}

async fn info(_app: Arc<App>, req: Request) -> ApiResult {
    Ok(json!({
        "default_source": "moonraker",
        "available_sources": ["moonraker"],
        "login_required": false,
        "trusted": req.access.trusted,
    }))
}

/// RFC 4648 base32 for input whose length is a multiple of 5, so no padding is needed.
pub fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(bytes.len() * 8 / 5);
    for chunk in bytes.chunks(5) {
        let n = chunk.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        for i in (0..8).rev() {
            out.push(ALPHABET[((n >> (i * 5)) & 31) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors(text: &str) -> Cors {
        let config = Config::parse(text).unwrap();
        let (auth, _) = crate::auth::Auth::new(&config).unwrap();
        let (cors, warnings) = Cors::new(&config, auth.trusted_networks());
        assert!(warnings.is_empty(), "{warnings:?}");
        cors
    }

    #[test]
    fn cors_domains() {
        let c = cors(
            "[authorization]\ncors_domains:\n  *.local\n  https://app.fluidd.xyz\n  http://*.lan:8080\n\
             trusted_clients:\n  192.168.1.0/24\n  10.1.2.3\n",
        );
        assert!(c.allows("http://bluev2.local"));
        assert!(c.allows("https://app.fluidd.xyz"));
        assert!(!c.allows("https://app.fluidd.xyz.evil.com"));
        assert!(c.allows("http://printer.lan:8080"));
        assert!(!c.allows("http://printer.lan"));
        assert!(c.allows("http://192.168.1.50:8080"));
        assert!(c.allows("http://10.1.2.3"));
        assert!(!c.allows("http://10.1.2.4"));
        // Without cors_domains, Moonraker sends no CORS headers at all.
        assert!(!cors("[authorization]\ntrusted_clients:\n  192.168.1.0/24\n").allows("http://192.168.1.50"));
        let (_, warnings) = Cors::new(
            &Config::parse("[authorization]\ncors_domains:\n  http://a.*\n  http://b.lan/\n").unwrap(),
            Vec::new(),
        );
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn encodes_base32() {
        assert_eq!(base32(b"fooba"), "MZXW6YTB");
        assert_eq!(base32(&[0; 20]).len(), 32);
    }
}
