//! The parts of Moonraker's `authorization.py` that frontends need when nobody logs in. orrery
//! has no authorization yet, so every client gets what Moonraker gives a trusted client: the
//! `_TRUSTED_USER_`. Oneshot tokens are handed out so frontends can build download links, and
//! they aren't checked. CORS follows `[authorization] cors_domains` and `trusted_clients`.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::json;

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::config::Config;
use crate::server::App;

const TRUSTED_USER: &str = "_TRUSTED_USER_";

/// When the trusted user was first seen, as Moonraker records it.
static TRUSTED_SINCE: OnceLock<f64> = OnceLock::new();

/// Which browser origins may call the API, after Moonraker's `check_cors`.
pub struct Cors {
    domains: Vec<Regex>,
    /// Trusted IPs and networks, as an address and prefix length.
    trusted: Vec<(IpAddr, u8)>,
}

impl Cors {
    /// Reads `[authorization]`. Returns the warnings Moonraker gives for options it skips.
    pub fn new(config: &Config) -> (Self, Vec<String>) {
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
        let trusted = section.get_list("trusted_clients").iter().filter_map(|c| parse_network(c)).collect();
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
        host.and_then(|h| h.parse::<IpAddr>().ok()).is_some_and(|ip| self.trusts(ip))
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|&(net, len)| in_network(ip, net, len))
    }
}

/// `10.0.0.0/8` or `127.0.0.1`. Host names, which Moonraker also accepts, are skipped.
fn parse_network(text: &str) -> Option<(IpAddr, u8)> {
    let (addr, len) = match text.split_once('/') {
        Some((a, l)) => (a.parse::<IpAddr>().ok()?, Some(l.parse::<u8>().ok()?)),
        None => (text.parse::<IpAddr>().ok()?, None),
    };
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let len = len.unwrap_or(max);
    (len <= max).then_some((addr, len))
}

fn in_network(ip: IpAddr, net: IpAddr, len: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
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
}

async fn user(_app: Arc<App>, req: Request) -> ApiResult {
    if req.kind != RequestType::Get {
        return Err(ApiError::bad_request("User accounts aren't available: orrery has no authorization yet"));
    }
    let since =
        *TRUSTED_SINCE.get_or_init(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64());
    Ok(json!({"username": TRUSTED_USER, "source": "moonraker", "created_on": since}))
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

async fn info(_app: Arc<App>, _req: Request) -> ApiResult {
    Ok(json!({
        "default_source": "moonraker",
        "available_sources": ["moonraker"],
        "login_required": false,
        "trusted": true,
    }))
}

/// RFC 4648 base32 for input whose length is a multiple of 5, so no padding is needed.
fn base32(bytes: &[u8]) -> String {
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
        let (cors, warnings) = Cors::new(&Config::from_str(text).unwrap());
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
        let (_, warnings) =
            Cors::new(&Config::from_str("[authorization]\ncors_domains:\n  http://a.*\n  http://b.lan/\n").unwrap());
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn encodes_base32() {
        assert_eq!(base32(b"fooba"), "MZXW6YTB");
        assert_eq!(base32(&[0; 20]).len(), 32);
    }
}
