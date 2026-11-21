//! The parts of Moonraker's `authorization.py` that frontends call: the `access.*` endpoints,
//! and CORS. Who may do what is in [`crate::auth`]; users and their tokens are in
//! [`crate::users`]. A client that isn't logged in is the `_TRUSTED_USER_`, or the
//! `_API_KEY_USER_` with a key, as in Moonraker. Oneshot tokens are handed out so frontends can
//! build download links, and they aren't checked yet. CORS follows `[authorization]
//! cors_domains` and the networks that have a role.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use regex::Regex;
use serde_json::{Value, json};

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::auth::Network;
use crate::config::Config;
use crate::server::App;
use crate::users::{LOCAL_SOURCE, TokenType, Users};

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
    r.register("/access/login", &[RequestType::Post], Transports::All, handler(login));
    r.register("/access/logout", &[RequestType::Post], Transports::All, handler(logout));
    r.register("/access/refresh_jwt", &[RequestType::Post], Transports::All, handler(refresh_jwt));
    r.register("/access/user/password", &[RequestType::Post], Transports::All, handler(user_password));
    r.register("/orrery/access/user/password", &[RequestType::Post], Transports::All, handler(set_password));
    r.register("/access/oneshot_token", &[RequestType::Get], Transports::All, handler(oneshot_token));
    r.register("/access/info", &[RequestType::Get], Transports::All, handler(info));
    r.register("/access/api_key", &[RequestType::Get, RequestType::Post], Transports::All, handler(api_key));
    r.register("/orrery/access/key/get", &[RequestType::Get], Transports::All, handler(key_get));
    r.register("/orrery/access/key/list", &[RequestType::Get], Transports::All, handler(key_list));
    r.register("/orrery/access/user/list", &[RequestType::Get], Transports::All, handler(user_list));
    r.register("/orrery/access/key/rotate", &[RequestType::Post], Transports::All, handler(key_rotate));
}

/// Fluidd's API key: `access.get_api_key` reads it and `access.post_api_key` makes a new one,
/// as in Moonraker. It's the `moonraker` key.
async fn api_key(app: Arc<App>, req: Request) -> ApiResult {
    let name = crate::keys::MOONRAKER_KEY;
    if !app.keys.exists(name) {
        return Err(ApiError::bad_request("orrery has no Moonraker API key: [moonraker_compat] legacy_api_key is off"));
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

/// `orrery.access.key.list`: every key's name and role, without the secrets.
async fn key_list(app: Arc<App>, _req: Request) -> ApiResult {
    let keys: Vec<Value> =
        app.keys.list().into_iter().map(|(name, role)| json!({"name": name, "role": role})).collect();
    Ok(json!({"keys": keys}))
}

/// `orrery.access.user.list`: the stored users with their roles from the config, which
/// `access.users.list` doesn't show. `role` is null for a user the config gives none.
async fn user_list(app: Arc<App>, _req: Request) -> ApiResult {
    let users: Vec<Value> = app
        .users
        .list()
        .into_iter()
        .map(|u| {
            json!({
                "username": u.username,
                "source": u.source,
                "role": app.users.role_of(&u.username).ok().map(crate::auth::Role::as_str),
                "created_on": u.created_on,
                "has_tokens": u.jwt_secret.is_some(),
            })
        })
        .collect();
    Ok(json!({"users": users}))
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
    ApiError::not_found(format!("No key named {name}: keys are listed by role in orrery.toml's [authorization.keys]"))
}

/// `access.get_user`, `access.post_user`, and `access.delete_user`.
async fn user(app: Arc<App>, req: Request) -> ApiResult {
    match req.kind {
        RequestType::Post => post_user(app, req).await,
        RequestType::Delete => delete_user(app, req).await,
        _ => get_user(&app, &req),
    }
}

/// The current user. As in Moonraker, a client that isn't logged in is the API key user with a
/// key, and the trusted user otherwise.
fn get_user(app: &App, req: &Request) -> ApiResult {
    if let Some(user) = req.access.user.as_deref().and_then(|name| app.users.get(name)) {
        return Ok(json!({
            "username": user.username,
            "source": app.compat.source_name(&user.source),
            "created_on": user.created_on,
        }));
    }
    let since = *TRUSTED_SINCE.get_or_init(crate::users::now_secs);
    let name = if req.access.key.is_some() { API_KEY_USER } else { TRUSTED_USER };
    Ok(json!({"username": name, "source": "moonraker", "created_on": since}))
}

fn local_logins_on(users: &Users) -> Result<(), ApiError> {
    if users.local_logins {
        return Ok(());
    }
    Err(ApiError::bad_request("Local passwords are off in orrery.toml (local_logins = false)"))
}

/// `access.post_user`: adds a local user, admin only. The user's role comes from the config.
async fn post_user(app: Arc<App>, req: Request) -> ApiResult {
    local_logins_on(&app.users)?;
    let name = req.args.str("username")?;
    let password = req.args.str("password")?;
    app.users.check_new_name(&name, |n| app.keys.exists(n))?;
    check_password_text(&password)?;
    let hash = hash_off_thread(password).await?;
    app.users.store(&app.database, Users::new_local(&name, hash))?;
    match app.users.role_of(&name).ok() {
        Some(role) => {
            tracing::info!(target: "orrery::auth", "Added the user {name}, with the {role} role, as {} asked", req.access.describe())
        }
        None => tracing::warn!(
            target: "orrery::auth",
            "Added the user {name}, as {} asked. orrery.toml gives them no role, so they can't log in until an admin lists them under [authorization.users]",
            req.access.describe()
        ),
    }
    let tokens = app.users.log_in(&app.database, &name)?;
    app.notify("user_created", vec![json!({"username": name})]);
    Ok(json!({
        "username": name,
        "token": tokens.token,
        "source": app.compat.source_name(LOCAL_SOURCE),
        "refresh_token": tokens.refresh_token,
        "action": "user_created",
    }))
}

/// `access.delete_user`: admin only. The user's tokens stop working, and their open
/// connections lose their rights.
async fn delete_user(app: Arc<App>, req: Request) -> ApiResult {
    let name = req.args.str("username")?;
    if req.access.user.as_deref() == Some(name.as_str()) {
        return Err(ApiError::bad_request(format!("You're logged in as {name}, so you can't delete {name}")));
    }
    if app.keys.exists(&name) || name.starts_with("agent.") {
        return Err(ApiError::bad_request(format!(
            "{name} is a key, not a user. To remove it, remove it from orrery.toml's [authorization.keys]"
        )));
    }
    if !app.users.remove(&app.database, &name)? {
        return Err(ApiError::not_found(format!("No user named {name}")));
    }
    tracing::info!(target: "orrery::auth", "Deleted the user {name}, as {} asked", req.access.describe());
    app.revoke_user(&name, None, "the user was deleted");
    app.notify("user_deleted", vec![json!({"username": name})]);
    Ok(json!({"username": name, "action": "user_deleted"}))
}

/// `access.users.list`: the stored users, and the keys, with source `orrery`. That includes
/// the `moonraker` key, which Moonraker hides as `_API_KEY_USER_`: a key that works should be
/// listed somewhere (user, 2026-11-21).
async fn users_list(app: Arc<App>, _req: Request) -> ApiResult {
    let mut users: Vec<Value> = app
        .users
        .list()
        .into_iter()
        .map(|u| json!({"username": u.username, "source": app.compat.source_name(&u.source), "created_on": u.created_on}))
        .collect();
    let since = *TRUSTED_SINCE.get_or_init(crate::users::now_secs);
    for name in app.keys.names() {
        users.push(json!({"username": name, "source": "orrery", "created_on": since}));
    }
    Ok(json!({"users": users}))
}

/// `access.login`: checks a local user's password, and logs the connection in.
async fn login(app: Arc<App>, req: Request) -> ApiResult {
    let name = req.args.str("username")?;
    let password = req.args.str("password")?;
    // orrery's name for local users and Moonraker's both work.
    let source = match req.args.opt_str("source")? {
        Some(name) => app.compat.parse_source(&name.to_lowercase()),
        None => LOCAL_SOURCE.to_owned(),
    };
    let client = describe_client(&req);
    let refuse = |why: &str| {
        tracing::info!(target: "orrery::auth", "Refused a login as {name} from {client}: {why}");
        ApiError::new(401, "Wrong user name or password")
    };
    if source != LOCAL_SOURCE {
        tracing::info!(target: "orrery::auth", "Refused a login as {name} from {client}: orrery has no {source} source");
        return Err(ApiError::new(401, format!("orrery has no {source} login source")));
    }
    local_logins_on(&app.users)?;
    let Some(user) = app.users.get(&name) else { return Err(refuse("no such user")) };
    if user.source != LOCAL_SOURCE {
        return Err(ApiError::new(
            401,
            format!("The user {name} logs in with the {} source", app.compat.source_name(&user.source)),
        ));
    }
    let Some(stored) = user.password.clone() else { return Err(refuse("the user has no password")) };
    let checked = {
        let password = password.clone();
        tokio::task::spawn_blocking(move || crate::users::check_password(&password, &stored))
            .await
            .map_err(|e| ApiError::from(anyhow::anyhow!("password check: {e}")))?
    };
    let (ok, outdated) = checked;
    if !ok {
        return Err(refuse("wrong password"));
    }
    let role = app.users.role_of(&name).inspect_err(|_| {
        tracing::info!(target: "orrery::auth", "Refused a login as {name} from {client}: orrery.toml gives the user no role");
    })?;
    if outdated {
        let mut user = user;
        user.password = Some(hash_off_thread(password).await?);
        app.users.store(&app.database, user)?;
        tracing::info!(target: "orrery::auth", "Replaced the user {name}'s imported password hash with an Argon2id one");
    }
    let tokens = app.users.log_in(&app.database, &name)?;
    if let Some(conn) = req.conn {
        app.set_client_user(conn, &name, role);
    }
    tracing::info!(target: "orrery::auth", "User {name} logged in from {client}, with the {role} role");
    Ok(json!({
        "username": name,
        "token": tokens.token,
        "source": app.compat.source_name(LOCAL_SOURCE),
        "refresh_token": tokens.refresh_token,
        "action": "user_logged_in",
    }))
}

/// `access.logout`: forgets the user's signing key, so their tokens stop working everywhere,
/// as in Moonraker.
async fn logout(app: Arc<App>, req: Request) -> ApiResult {
    let Some(name) = req.access.user.clone() else {
        return Err(ApiError::bad_request("No user is logged in on this connection"));
    };
    app.users.forget_key(&app.database, &name)?;
    app.revoke_user(&name, None, "the user logged out");
    tracing::info!(target: "orrery::auth", "User {name} logged out, from {}", describe_client(&req));
    app.notify("user_logged_out", vec![json!({"username": name})]);
    Ok(json!({"username": name, "action": "user_logged_out"}))
}

/// `access.refresh_jwt`: a new access token for a refresh token.
async fn refresh_jwt(app: Arc<App>, req: Request) -> ApiResult {
    let refresh = req.args.str("refresh_token")?;
    let name = app.users.check_token(refresh.trim(), TokenType::Refresh, true).map_err(|e| {
        tracing::info!(target: "orrery::auth", "Refused a token refresh from {}: {}", describe_client(&req), e.message);
        ApiError::new(401, format!("Invalid Refresh Token: {}", e.message))
    })?;
    let token = app.users.access_token(&name).map_err(|_| ApiError::new(401, "User not logged in"))?;
    let source = app.users.get(&name).map(|u| u.source).unwrap_or_default();
    Ok(
        json!({"username": name, "token": token, "source": app.compat.source_name(&source), "action": "user_jwt_refresh"}),
    )
}

/// `access.user.password`: a user changes their own password. Their other sessions end; this
/// connection stays logged in.
async fn user_password(app: Arc<App>, req: Request) -> ApiResult {
    let Some(name) = req.access.user.clone() else {
        return Err(ApiError::bad_request("No user is logged in on this connection"));
    };
    let password = req.args.str("password")?;
    let new_password = req.args.str("new_password")?;
    let user = app.users.get(&name).ok_or_else(|| ApiError::not_found(format!("No user named {name}")))?;
    let Some(stored) = user.password.clone().filter(|_| user.source == LOCAL_SOURCE) else {
        return Err(ApiError::bad_request(format!(
            "The user {name} logs in with the {} source, which keeps the password",
            user.source
        )));
    };
    check_password_text(&new_password)?;
    let (ok, _) = tokio::task::spawn_blocking(move || crate::users::check_password(&password, &stored))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("password check: {e}")))?;
    if !ok {
        return Err(ApiError::bad_request("The current password is wrong"));
    }
    replace_password(&app, &name, new_password, req.conn).await?;
    tracing::info!(target: "orrery::auth", "User {name} changed their password, from {}", describe_client(&req));
    Ok(json!({"username": name, "action": "user_password_reset"}))
}

/// `orrery.access.user.password {username, password}`: an admin sets a user's password, as
/// `orrery user password` does.
async fn set_password(app: Arc<App>, req: Request) -> ApiResult {
    let name = req.args.str("username")?;
    let password = req.args.str("password")?;
    let user = app.users.get(&name).ok_or_else(|| ApiError::not_found(format!("No user named {name}")))?;
    if user.source != LOCAL_SOURCE {
        return Err(ApiError::bad_request(format!("The user {name} logs in with the {} source", user.source)));
    }
    check_password_text(&password)?;
    replace_password(&app, &name, password, req.conn).await?;
    tracing::info!(target: "orrery::auth", "Set the user {name}'s password, as {} asked", req.access.describe());
    Ok(json!({"username": name, "action": "user_password_reset"}))
}

/// Stores a new password and a new signing key, so the user's other sessions end. `keep`
/// stays logged in.
async fn replace_password(
    app: &App,
    name: &str,
    password: String,
    keep: Option<crate::api::ConnId>,
) -> Result<(), ApiError> {
    let hash = hash_off_thread(password).await?;
    let mut user = app.users.get(name).ok_or_else(|| ApiError::not_found(format!("No user named {name}")))?;
    user.password = Some(hash);
    let had_key = user.jwt_secret.is_some();
    user.jwt_secret = None;
    user.jwk_id = None;
    app.users.store(&app.database, user)?;
    if had_key {
        app.users.log_in(&app.database, name)?;
    }
    app.revoke_user(name, keep, "the user's password changed");
    Ok(())
}

fn check_password_text(password: &str) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(ApiError::bad_request("A password can't be empty"));
    }
    Ok(())
}

async fn hash_off_thread(password: String) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || crate::users::hash_password(&password))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("password hash: {e}")))?
        .map_err(ApiError::from)
}

/// Who asked, for log lines: the client's address.
fn describe_client(req: &Request) -> String {
    if req.access.client.is_empty() { req.access.source.clone() } else { req.access.client.clone() }
}

async fn oneshot_token(_app: Arc<App>, _req: Request) -> ApiResult {
    // Moonraker: base32 of 20 random bytes.
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..4]);
    Ok(json!(base32(&bytes)))
}

async fn info(app: Arc<App>, req: Request) -> ApiResult {
    let local = app.compat.source_name(LOCAL_SOURCE);
    let sources: Vec<&str> = if app.users.local_logins { vec![local] } else { vec![] };
    Ok(json!({
        "default_source": local,
        "available_sources": sources,
        "login_required": false,
        "trusted": req.access.trusted,
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
