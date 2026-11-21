//! Authorization: where a request comes from, and what it may do. See
//! docs/design/2026-11-20-authorization.md.
//!
//! Every request carries an [`Access`]: a [`Role`], and the agent it speaks for, if any. Each
//! endpoint has a [`Need`], from [`need`]. An endpoint that the table doesn't list needs
//! `admin`.
//!
//! Where access comes from:
//! - The unix socket: orrery's own user and root are `admin`. Other users get nothing, unless
//!   they connect as a configured agent that names them.
//! - TCP: the role of the client's network, by its address. A trusted proxy passes on that
//!   address in `X-Real-Ip` or `X-Forwarded-For`; anyone else's headers are ignored. A browser
//!   request whose `Origin` isn't allowed gets nothing from its network.
//! - A key, sent as `X-Api-Key` or as `api_key` in `server.connection.identify`.

use std::collections::HashSet;
use std::fmt;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use axum::http::HeaderMap;
use serde_json::Value;

use crate::api::{ApiError, Args, RequestType};
use crate::config::Config;
use crate::server::App;

/// How often repeated refusals are summed up in the log.
const REFUSAL_SUMMARY: Duration = Duration::from_secs(3600);

/// What a client may do. Each role includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "viewer" => Some(Self::Viewer),
            "operator" => Some(Self::Operator),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a connection or request may do, and why.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Access {
    /// `None`: no rights at all.
    pub role: Option<Role>,
    /// The agent this connection speaks for. It may use what that agent owns.
    pub agent: Option<String>,
    /// The rights come from the client's network, as `access.info` reports `trusted`.
    pub trusted: bool,
    /// The rights come from a key, as `access.get_user` reports.
    pub key: Option<String>,
    /// Why, for the log: "network 192.168.1.0/24", "key homeassistant", and so on.
    pub source: String,
}

impl Access {
    pub fn none(source: impl Into<String>) -> Self {
        Self { source: source.into(), ..Self::default() }
    }

    pub fn admin(source: impl Into<String>) -> Self {
        Self { role: Some(Role::Admin), source: source.into(), ..Self::default() }
    }

    /// The role and its source, for log lines.
    pub fn describe(&self) -> String {
        match (self.role, &self.agent) {
            (None, _) => format!("no rights ({})", self.source),
            (Some(role), Some(agent)) => format!("{role} as agent {agent} ({})", self.source),
            (Some(role), None) => format!("{role} ({})", self.source),
        }
    }
}

/// What an endpoint needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Need {
    /// Any client, even one with no rights: `access.info` and `server.connection.identify`.
    Anyone,
    Role(Role),
    /// The agent of this name, or `admin`: its database namespace, its own unit.
    Owner(String),
    /// Calling the agent of this name: its `call_role`, `operator` by default.
    Call(String),
}

/// Checks a request's access against what its endpoint needs.
pub fn check(app: &App, access: &Access, need: &Need) -> Result<(), ApiError> {
    if *need == Need::Anyone {
        return Ok(());
    }
    let Some(role) = access.role else {
        return Err(ApiError::new(401, "Unauthorized"));
    };
    let needed = match need {
        Need::Anyone => return Ok(()),
        Need::Role(r) => *r,
        Need::Owner(agent) => {
            if access.agent.as_deref() == Some(agent.as_str()) {
                return Ok(());
            }
            Role::Admin
        }
        Need::Call(agent) => app.agents.call_role(agent),
    };
    if role >= needed {
        Ok(())
    } else {
        Err(ApiError::new(403, format!("Forbidden: this needs the {needed} role, and the client has {role}")))
    }
}

/// What an endpoint needs. `endpoint` is the path as registered: `/server/info`, or a Klippy
/// endpoint such as `gcode/script`, which has no leading slash.
pub fn need(app: &App, endpoint: &str, kind: RequestType, args: &Args) -> Need {
    use Role::*;
    if !endpoint.starts_with('/') {
        return klippy_need(endpoint);
    }
    if let Some(rest) = endpoint.strip_prefix("/agent/") {
        return Need::Call(rest.split('/').next().unwrap_or_default().to_owned());
    }
    if let Some(rest) = endpoint.strip_prefix("/server/files/") {
        return files_need(rest, kind, args);
    }
    let get = kind == RequestType::Get;
    let role = match endpoint {
        "/access/info" | "/server/connection/identify" => return Need::Anyone,
        "/server/info"
        | "/server/config"
        | "/server/websocket/id"
        | "/server/temperature_store"
        | "/server/gcode_store"
        | "/machine/system_info"
        | "/machine/proc_stats"
        | "/server/extensions/list"
        | "/access/oneshot_token"
        | "/server/history/list"
        | "/server/history/totals"
        | "/server/job_queue/status"
        | "/server/webcams/list"
        | "/server/database/list" => Viewer,
        "/access/user" if get => Viewer,
        "/server/history/job" | "/server/webcams/item" if get => Viewer,
        "/server/history/job"
        | "/server/history/reset_totals"
        | "/server/webcams/item"
        | "/server/webcams/test"
        | "/server/logs/rollover"
        | "/printer/print/start"
        | "/printer/print/pause"
        | "/printer/print/resume"
        | "/printer/print/cancel"
        | "/printer/restart"
        | "/printer/firmware_restart" => Operator,
        e if e.starts_with("/server/job_queue/") => Operator,
        // An agent's own calls. What they reach is the agent's own, or checked again: history
        // fields against the agent's table.
        "/connection/register_remote_method" | "/connection/send_event" => Viewer,
        e if e.starts_with("/orrery/agent/") => Viewer,
        "/server/extensions/request" => {
            return Need::Call(args.get("agent").and_then(Value::as_str).unwrap_or_default().to_owned());
        }
        "/server/database/item" => return database_need(app, kind, args),
        e if e.starts_with("/machine/services/") => return service_need(app, e, args),
        _ => Admin,
    };
    Need::Role(role)
}

/// Klipper's own endpoints (`klippy/` in Klipper's source, `register_endpoint`).
fn klippy_need(endpoint: &str) -> Need {
    let role = match endpoint {
        "info"
        | "objects/list"
        | "objects/query"
        | "objects/subscribe"
        | "gcode/help"
        | "query_endstops/status"
        | "bed_mesh/dump_mesh" => Role::Viewer,
        "gcode/script"
        | "emergency_stop"
        | "pause_resume/pause"
        | "pause_resume/resume"
        | "pause_resume/cancel"
        | "gcode/restart"
        | "gcode/firmware_restart" => Role::Operator,
        // From a Klipper extension: orrery can't know what it does.
        _ => Role::Admin,
    };
    Need::Role(role)
}

/// Reading a file root: gcodes for everyone, logs and config for operators.
fn read_role(root: &str) -> Role {
    match root {
        "gcodes" => Role::Viewer,
        "logs" | "config" => Role::Operator,
        _ => Role::Admin,
    }
}

/// Changing a file root: gcodes for operators. The config root is admin's: an `orrery.toml`
/// there can set an agent's `command`.
fn write_role(root: &str) -> Role {
    match root {
        "gcodes" => Role::Operator,
        _ => Role::Admin,
    }
}

/// The root a request path names, such as `gcodes` for `gcodes/a/../b.gcode`.
fn root_of(path: &str) -> String {
    let norm = crate::files::normalize(std::path::Path::new(path));
    norm.to_string_lossy().trim_matches('/').split('/').next().unwrap_or_default().to_owned()
}

fn str_arg(args: &Args, key: &str) -> Option<String> {
    args.get(key).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
}

/// `/server/files/<rest>`: the file endpoints, and downloads and deletes of files by path.
fn files_need(rest: &str, kind: RequestType, args: &Args) -> Need {
    let role = match rest {
        "roots" => Role::Viewer,
        "list" => read_role(&str_arg(args, "root").unwrap_or_else(|| "gcodes".into())),
        "metadata" | "thumbnails" => Role::Viewer,
        "metascan" => Role::Operator,
        "upload" => write_role(&str_arg(args, "root").unwrap_or_else(|| "gcodes".into())),
        "directory" => {
            let root = root_of(&str_arg(args, "path").unwrap_or_else(|| "gcodes".into()));
            if kind == RequestType::Get { read_role(&root) } else { write_role(&root) }
        }
        "delete_file" => write_role(&root_of(&str_arg(args, "path").unwrap_or_default())),
        "move" | "copy" => {
            let source = root_of(&str_arg(args, "source").unwrap_or_default());
            let dest = root_of(&str_arg(args, "dest").unwrap_or_default());
            let source = if rest == "move" { write_role(&source) } else { read_role(&source) };
            source.max(write_role(&dest))
        }
        "zip" => {
            let dest = str_arg(args, "dest").map_or_else(|| "config".into(), |d| root_of(&d));
            let items = args.get("items").map(crate::files::string_list).unwrap_or_default();
            items.iter().map(|i| read_role(&root_of(i))).fold(write_role(&dest), Role::max)
        }
        // Moonraker serves these two logs at fixed addresses.
        "klippy.log" | "moonraker.log" => Role::Operator,
        path => {
            let root = root_of(path);
            if kind == RequestType::Delete { write_role(&root) } else { read_role(&root) }
        }
    };
    Need::Role(role)
}

/// The database: agents' namespaces are their own; the rest is readable by viewers and
/// writable by operators, as frontends keep their settings there.
fn database_need(app: &App, kind: RequestType, args: &Args) -> Need {
    let namespace = str_arg(args, "namespace").unwrap_or_default();
    if app.agents.is_configured(&namespace) {
        return Need::Owner(namespace);
    }
    match (kind, namespace.as_str()) {
        (RequestType::Get, _) => Need::Role(Role::Viewer),
        // Moonraker's own state.
        (_, "moonraker") => Need::Role(Role::Admin),
        _ => Need::Role(Role::Operator),
    }
}

/// `machine.services.*`: admin's, except that an agent may restart its own unit.
fn service_need(app: &App, endpoint: &str, args: &Args) -> Need {
    let service = str_arg(args, "service").unwrap_or_default();
    match app.agents.agent_of_service(&service) {
        Some(agent) if endpoint.ends_with("/restart") => Need::Owner(agent),
        _ => Need::Role(Role::Admin),
    }
}

/// A network and its prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    pub addr: IpAddr,
    pub len: u8,
}

impl Network {
    /// `10.0.0.0/8` or `127.0.0.1`.
    pub fn parse(text: &str) -> Option<Self> {
        let (addr, len) = match text.split_once('/') {
            Some((a, l)) => (a.trim().parse::<IpAddr>().ok()?, Some(l.trim().parse::<u8>().ok()?)),
            None => (text.trim().parse::<IpAddr>().ok()?, None),
        };
        let addr = addr.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let len = len.unwrap_or(max);
        (len <= max).then_some(Self { addr, len })
    }

    /// The network's first address. An entry written with the host part set, such as
    /// `192.168.1.5/24`, is the network `192.168.1.0/24`.
    fn base(&self) -> IpAddr {
        match self.addr {
            IpAddr::V4(a) => {
                IpAddr::V4((u32::from(a) & u32::MAX.checked_shl(32 - u32::from(self.len)).unwrap_or(0)).into())
            }
            IpAddr::V6(a) => {
                IpAddr::V6((u128::from(a) & u128::MAX.checked_shl(128 - u32::from(self.len)).unwrap_or(0)).into())
            }
        }
    }

    /// Whether two entries name the same network: `192.168.1.0/24` twice, or once written with
    /// the host part set, as `192.168.1.5/24`.
    fn same_as(&self, other: &Network) -> bool {
        self.len == other.len && self.base() == other.base()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (ip.to_canonical(), self.addr) {
            (IpAddr::V4(ip), IpAddr::V4(net)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.len)).unwrap_or(0);
                u32::from(ip) & mask == u32::from(net) & mask
            }
            (IpAddr::V6(ip), IpAddr::V6(net)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(self.len)).unwrap_or(0);
                u128::from(ip) & mask == u128::from(net) & mask
            }
            _ => false,
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let max = if self.addr.is_ipv4() { 32 } else { 128 };
        // Always the network's first address, so `192.168.1.5/24` shows as `192.168.1.0/24`.
        if self.len == max { write!(f, "{}", self.addr) } else { write!(f, "{}/{}", self.base(), self.len) }
    }
}

/// orrery's authorization settings, from `[authorization]` and `[server] use_xheaders`.
pub struct Auth {
    /// Networks and their roles: `[authorization.networks]`, or Moonraker's `trusted_clients`
    /// as `admin`.
    trusted: Vec<(Network, Role)>,
    /// Where `trusted` came from, for the log.
    networks_from: &'static str,
    /// `trusted_clients` was set, and `[authorization.networks]` replaced it.
    ignored_trusted_clients: bool,
    /// Hosts whose proxy headers orrery reads.
    proxies: Vec<Network>,
    /// The proxies came from `trusted_proxies`, not from `trusted_clients`.
    explicit_proxies: bool,
    use_xheaders: bool,
    /// orrery's own user id, which is admin on the unix socket.
    own_uid: u32,
    /// What has been logged once already, so repeats don't fill the log.
    logged: Mutex<HashSet<String>>,
    /// Refusals since the last hourly total, and when that total was logged.
    refusals: Mutex<(u64, Instant)>,
}

impl Auth {
    /// Reads the settings. Returns warnings for entries orrery skips.
    pub fn new(config: &Config) -> Result<(Self, Vec<String>)> {
        let mut warnings = Vec::new();
        let section = config.section("authorization");
        if config.section_json("authorization trusted_clients").is_some() {
            bail!(
                "[authorization.trusted_clients] is now [authorization.networks], which lists the \
                 networks for each role, such as admin = [\"192.168.1.0/24\"] and viewer = [\"192.168.1.134\"]."
            );
        }
        let mut trusted: Vec<(Network, Role)> = Vec::new();
        let networks = config.section_json("authorization networks");
        let networks_from = if networks.is_some() { "[authorization.networks]" } else { "trusted_clients" };
        let ignored_trusted_clients = networks.is_some() && section.has("trusted_clients");
        if let Some(table) = networks {
            // orrery.toml's form: the networks for each role, such as admin = ["192.168.1.0/24"].
            for (role_name, list) in table {
                let Some(role) = Role::parse(&role_name) else {
                    bail!(
                        "[authorization.networks]: {role_name} isn't a role. Use viewer, operator, or admin, \
                         such as operator = [\"192.168.1.0/24\"]"
                    );
                };
                let entries: Vec<&Value> = match &list {
                    Value::Array(items) => items.iter().collect(),
                    other => vec![other],
                };
                for entry in entries {
                    let Some(net) = entry.as_str() else {
                        bail!("[authorization.networks] {role_name}: {entry} isn't an IP address or network in quotes");
                    };
                    let Some(network) = Network::parse(net) else {
                        bail!(
                            "[authorization.networks] {role_name}: {net:?} isn't an IP address or network, such as \
                             192.168.1.0/24"
                        );
                    };
                    if let Some((other, other_role)) = trusted.iter().find(|(n, _)| n.same_as(&network)) {
                        bail!(
                            "[authorization.networks]: {net} ({role_name}) and {other} ({other_role}) are the same \
                             network, so it's unclear which role it gets. Keep one of them."
                        );
                    }
                    trusted.push((network, role));
                }
            }
        } else {
            // Moonraker's form, a list: each network gets admin, as in Moonraker.
            for entry in section.get_list("trusted_clients") {
                match Network::parse(&entry) {
                    Some(network) => trusted.push((network, Role::Admin)),
                    None => warnings.push(format!(
                        "[authorization] trusted_clients: orrery doesn't look up host names yet, so it skips {entry:?}"
                    )),
                }
            }
        }
        // Proxies are only what trusted_proxies lists. Moonraker's trusted_clients alone keeps
        // Moonraker's behaviour, where its networks may pass on proxy headers.
        let explicit_proxies = section.has("trusted_proxies");
        let proxies = if explicit_proxies {
            let mut proxies = Vec::new();
            for entry in section.get_list("trusted_proxies") {
                match Network::parse(&entry) {
                    Some(n) => proxies.push(n),
                    None => warnings.push(format!(
                        "[authorization] trusted_proxies: {entry:?} isn't an IP address or network, so orrery skips it"
                    )),
                }
            }
            proxies
        } else if networks_from == "trusted_clients" {
            trusted.iter().map(|(n, _)| *n).collect()
        } else {
            Vec::new()
        };
        // A proxy that drops the headers would hand its users its own address's role. So a
        // network entry can't lie inside a proxy, and a proxy gets nothing from a network
        // around it (see network_access).
        if explicit_proxies {
            for (network, _) in &trusted {
                if let Some(proxy) = proxies.iter().find(|p| p.len <= network.len && p.contains(network.addr)) {
                    bail!(
                        "{networks_from}: {network} is a trusted proxy ({proxy} in trusted_proxies), and a \
                         proxy can't have a role of its own. Remove {network} from {networks_from}."
                    );
                }
            }
        }
        let use_xheaders = config.section("server").get_bool("use_xheaders", true)?;
        let own_uid = rustix::process::geteuid().as_raw();
        Ok((
            Self {
                trusted,
                networks_from,
                ignored_trusted_clients,
                proxies,
                explicit_proxies,
                use_xheaders,
                own_uid,
                logged: Mutex::new(HashSet::new()),
                refusals: Mutex::new((0, Instant::now())),
            },
            warnings,
        ))
    }

    /// The networks that have a role, for CORS: an origin whose host is such an address is
    /// allowed.
    pub fn trusted_networks(&self) -> Vec<Network> {
        self.trusted.iter().map(|(n, _)| *n).collect()
    }

    /// The startup log lines.
    pub fn log_settings(&self) {
        if self.ignored_trusted_clients {
            tracing::info!(target: "orrery::auth", "Authorization: [authorization.networks] replaces trusted_clients, so orrery ignores trusted_clients");
        }
        let from = self.networks_from;
        if self.trusted.is_empty() {
            tracing::info!(target: "orrery::auth", "Authorization: no networks have a role, so clients need a key");
        } else {
            let list: Vec<String> = self.trusted.iter().map(|(n, r)| format!("{n} ({r})")).collect();
            tracing::info!(target: "orrery::auth", "Authorization: networks, from {from}: {}", list.join(", "));
        }
        let from = if self.explicit_proxies { "trusted_proxies" } else { "trusted_clients" };
        if !self.use_xheaders {
            tracing::info!(target: "orrery::auth", "Authorization: proxy headers are off (use_xheaders: False)");
        } else if self.proxies.is_empty() {
            tracing::info!(target: "orrery::auth", "Authorization: no trusted proxies, so orrery reads no proxy headers");
        } else {
            let list: Vec<String> = self.proxies.iter().map(ToString::to_string).collect();
            tracing::info!(target: "orrery::auth", "Authorization: trusted proxies, from {from}: {}", list.join(", "));
            if self.explicit_proxies {
                tracing::info!(target: "orrery::auth", "Authorization: trusted proxies get no role from a network; their users get theirs from the proxy headers");
            }
        }
    }

    /// Logs a line once per orrery run for each `key`.
    pub fn log_once(&self, key: String, line: impl FnOnce() -> String) {
        if self.logged.lock().expect("logged lock").insert(key) {
            tracing::warn!(target: "orrery::auth", "{}", line());
        }
    }

    /// Logs a refused request: the first time for each client and reason, and after that an
    /// hourly total while refusals keep coming.
    pub fn log_refusal(&self, access: &Access, method: &str, error: &ApiError) {
        let first =
            self.logged.lock().expect("logged lock").insert(format!("refused {} {}", access.source, error.message));
        if first {
            tracing::warn!(target: "orrery::auth", "Refused {method}: {} ({}). Later refusals like it are only counted.", error.message, access.source);
            return;
        }
        let mut refusals = self.refusals.lock().expect("refusals lock");
        refusals.0 += 1;
        if refusals.1.elapsed() >= REFUSAL_SUMMARY {
            tracing::warn!(target: "orrery::auth", "Refused {} more requests in the last hour", refusals.0);
            *refusals = (0, Instant::now());
        }
    }

    fn is_proxy(&self, ip: IpAddr) -> bool {
        self.use_xheaders && self.proxies.iter().any(|n| n.contains(ip))
    }

    /// The client's address: the one a trusted proxy passes on, or the connection's own.
    pub fn client_address(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = peer.to_canonical();
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
        let real_ip = header("x-real-ip").filter(|v| !v.is_empty());
        // The last address: the one the proxy added. Earlier ones are the client's own words.
        let forwarded = header("x-forwarded-for").and_then(|v| v.rsplit(',').next()).map(str::trim);
        let Some(claimed) = real_ip.or(forwarded).filter(|v| !v.is_empty()) else {
            if self.explicit_proxies && self.is_proxy(peer) {
                self.log_once(format!("no-headers {peer}"), || {
                    format!("Trusted proxy {peer} sent a request without X-Real-Ip or X-Forwarded-For, so it's judged by the proxy's own address")
                });
            }
            return peer;
        };
        if !self.is_proxy(peer) {
            if self.use_xheaders {
                self.log_once(format!("untrusted-headers {peer}"), || {
                    format!("{peer} sent proxy headers, but it isn't a trusted proxy, so orrery ignores them")
                });
            }
            return peer;
        }
        match claimed.parse::<IpAddr>() {
            Ok(ip) => ip.to_canonical(),
            Err(_) => {
                self.log_once(format!("bad-header {peer}"), || {
                    format!("Trusted proxy {peer} passed on {claimed:?}, which isn't an IP address, so it's judged by the proxy's own address")
                });
                peer
            }
        }
    }

    /// Access for a TCP request or websocket, from the client's network. `origin_ok` is false
    /// for a browser request from a site that isn't allowed.
    pub fn network_access(&self, client: IpAddr, origin_ok: bool) -> Access {
        if self.explicit_proxies && self.proxies.iter().any(|p| p.contains(client)) {
            return Access::none(format!("{client} is a trusted proxy, which gets no role from a network"));
        }
        // The most specific network decides, as in a routing table, so a single address can get
        // less than the network around it, or more. No two entries are the same network.
        let best = self.trusted.iter().filter(|(n, _)| n.contains(client)).max_by_key(|(n, r)| (n.len, *r));
        let Some((network, role)) = best else {
            return Access::none(format!("{client} isn't in a network with a role"));
        };
        if !origin_ok {
            return Access::none(format!("{client} is in network {network}, but the request's Origin isn't allowed"));
        }
        Access { role: Some(*role), trusted: true, source: format!("network {network}"), ..Access::default() }
    }

    /// Access over the unix socket, by the peer's user id.
    pub fn unix_access(&self, uid: Option<u32>) -> Access {
        match uid {
            Some(uid) if uid == self.own_uid || uid == 0 => Access::admin(format!("unix socket, user {uid}")),
            Some(uid) => Access::none(format!("unix socket, user {uid}")),
            None => Access::none("unix socket, unknown user"),
        }
    }
}

/// Whether a browser request's `Origin` is allowed: the site it was sent to (its `Host`), or a
/// domain in `cors_domains`. A request without `Origin` isn't from another site's page.
pub fn origin_allowed(headers: &HeaderMap, cors: &crate::access::Cors) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return headers.get("origin").is_none();
    };
    let authority = origin.split_once("://").map(|(_, a)| a.trim_end_matches('/'));
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    if authority.is_some() && authority == host {
        return true;
    }
    cors.allows(origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn auth(text: &str) -> Auth {
        let (auth, warnings) = Auth::new(&Config::parse(text).unwrap()).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        auth
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn networks() {
        let n = Network::parse("192.168.1.0/24").unwrap();
        assert!(n.contains(ip("192.168.1.50")));
        assert!(!n.contains(ip("192.168.2.1")));
        assert!(n.contains(ip("::ffff:192.168.1.7")));
        assert!(Network::parse("10.0.0.1").unwrap().contains(ip("10.0.0.1")));
        assert!(Network::parse("10.0.0.0/33").is_none());
        assert!(Network::parse("printer.local").is_none());
        assert_eq!(Network::parse("fe80::/10").unwrap().to_string(), "fe80::/10");
        // An entry with the host part set is the network it's in.
        let n = Network::parse("192.168.1.5/24").unwrap();
        assert_eq!(n.to_string(), "192.168.1.0/24");
        assert!(n.same_as(&Network::parse("192.168.1.0/24").unwrap()));
    }

    #[test]
    fn moonraker_list_gives_admin() {
        let a = auth("[authorization]\ntrusted_clients:\n  192.168.1.0/24\n");
        assert_eq!(a.network_access(ip("192.168.1.9"), true).role, Some(Role::Admin));
        assert_eq!(a.network_access(ip("10.0.0.1"), true).role, None);
        assert_eq!(a.network_access(ip("192.168.1.9"), false).role, None);
    }

    #[test]
    fn toml_roles() {
        let mut c = Config::default();
        c.parse_toml(
            "[authorization.networks]\noperator = [\"192.168.1.0/24\"]\nadmin = \"192.168.1.10\"\n",
            std::path::Path::new("orrery.toml"),
        )
        .unwrap();
        let (a, _) = Auth::new(&c).unwrap();
        assert_eq!(a.network_access(ip("192.168.1.9"), true).role, Some(Role::Operator));
        assert_eq!(a.network_access(ip("192.168.1.10"), true).role, Some(Role::Admin));

        // A single address can also get less than its network.
        let mut c = Config::default();
        c.parse_toml(
            "[authorization.networks]\noperator = [\"192.168.1.134\"]\nadmin = [\"192.168.1.0/24\", \"10.0.0.0/8\"]\n",
            std::path::Path::new("orrery.toml"),
        )
        .unwrap();
        let (a, _) = Auth::new(&c).unwrap();
        assert_eq!(a.network_access(ip("192.168.1.134"), true).role, Some(Role::Operator));
        assert_eq!(a.network_access(ip("192.168.1.9"), true).role, Some(Role::Admin));
        assert_eq!(a.network_access(ip("10.1.1.1"), true).role, Some(Role::Admin));

        // A name that isn't a role is an error.
        let e = toml_auth("[authorization.networks]\nagent = [\"10.0.0.0/8\"]\n").err().unwrap().to_string();
        assert!(e.contains("isn't a role"), "{e}");

        // Two entries for one network are an error, also when one is written with the host part
        // set.
        let mut c = Config::default();
        c.parse_toml(
            "[authorization.networks]\nadmin = [\"192.168.1.0/24\"]\noperator = [\"192.168.1.5/24\"]\n",
            std::path::Path::new("orrery.toml"),
        )
        .unwrap();
        let e = Auth::new(&c).err().unwrap().to_string();
        assert!(e.contains("are the same network"), "{e}");

        let mut c = Config::default();
        c.parse_toml("[authorization.networks]\nviewer = [\"10.0.0.0/8\"]\n", std::path::Path::new("orrery.toml"))
            .unwrap();
        let (a, _) = Auth::new(&c).unwrap();
        assert_eq!(a.network_access(ip("10.1.2.3"), true).role, Some(Role::Viewer));
        // The old name is an error that says the new one.
        let mut c = Config::default();
        c.parse_toml(
            "[authorization.trusted_clients]\n\"10.0.0.0/8\" = \"admin\"\n",
            std::path::Path::new("orrery.toml"),
        )
        .unwrap();
        assert!(Auth::new(&c).err().unwrap().to_string().contains("[authorization.networks]"));
    }

    #[test]
    fn proxy_headers() {
        let a = auth("[authorization]\ntrusted_clients:\n  127.0.0.1\n  192.168.1.0/24\n");
        let h = headers(&[("x-forwarded-for", "10.9.9.9, 192.168.1.5")]);
        // From a trusted proxy, the last forwarded address counts.
        assert_eq!(a.client_address(ip("127.0.0.1"), &h), ip("192.168.1.5"));
        // X-Real-Ip wins, as in Tornado.
        let h2 = headers(&[("x-real-ip", "8.8.8.8"), ("x-forwarded-for", "192.168.1.5")]);
        assert_eq!(a.client_address(ip("127.0.0.1"), &h2), ip("8.8.8.8"));
        // From anyone else, the headers are ignored.
        assert_eq!(a.client_address(ip("10.1.1.1"), &h), ip("10.1.1.1"));
        // Without headers, the proxy is judged by its own address.
        assert_eq!(a.client_address(ip("127.0.0.1"), &HeaderMap::new()), ip("127.0.0.1"));
        let off = auth("[server]\nuse_xheaders: False\n[authorization]\ntrusted_clients:\n  127.0.0.1\n");
        assert_eq!(off.client_address(ip("127.0.0.1"), &h), ip("127.0.0.1"));
    }

    #[test]
    fn explicit_proxies() {
        let a = auth("[authorization]\ntrusted_clients:\n  192.168.1.0/24\ntrusted_proxies:\n  127.0.0.1\n");
        let h = headers(&[("x-real-ip", "192.168.1.5")]);
        let client = a.client_address(ip("127.0.0.1"), &h);
        assert_eq!(a.network_access(client, true).role, Some(Role::Admin));
        // The proxy's own address grants nothing.
        let client = a.client_address(ip("127.0.0.1"), &HeaderMap::new());
        assert_eq!(a.network_access(client, true).role, None);
    }

    fn toml_auth(toml: &str) -> Result<Auth> {
        let mut c = Config::default();
        c.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
        Auth::new(&c).map(|(a, _)| a)
    }

    #[test]
    fn networks_and_proxies() {
        // With [authorization.networks], no proxy is trusted unless trusted_proxies says so.
        let a = toml_auth("[authorization.networks]\nadmin = [\"127.0.0.0/8\"]\n").unwrap();
        let h = headers(&[("x-real-ip", "10.1.1.1")]);
        assert_eq!(a.client_address(ip("127.0.0.1"), &h), ip("127.0.0.1"));

        // A proxy inside a network gets nothing itself; its users are judged by the headers.
        let toml = "[authorization]\ntrusted_proxies = [\"192.168.1.10\"]\n\
                    [authorization.networks]\noperator = [\"192.168.1.0/24\"]\n";
        let a = toml_auth(toml).unwrap();
        assert_eq!(a.network_access(ip("192.168.1.10"), true).role, None);
        assert_eq!(a.network_access(ip("192.168.1.11"), true).role, Some(Role::Operator));
        let h = headers(&[("x-real-ip", "192.168.1.20")]);
        let client = a.client_address(ip("192.168.1.10"), &h);
        assert_eq!(a.network_access(client, true).role, Some(Role::Operator));

        // A network entry inside a proxy is an error.
        let toml = "[authorization]\ntrusted_proxies = [\"127.0.0.0/8\"]\n\
                    [authorization.networks]\nadmin = [\"127.0.0.1\"]\n";
        let e = toml_auth(toml).err().unwrap().to_string();
        assert!(e.contains("is a trusted proxy"), "{e}");
    }

    #[test]
    fn origins() {
        let (cors, _) = crate::access::Cors::new(
            &Config::parse("[authorization]\ncors_domains:\n  https://app.fluidd.xyz\n").unwrap(),
            Vec::new(),
        );
        assert!(origin_allowed(&HeaderMap::new(), &cors));
        assert!(origin_allowed(&headers(&[("origin", "http://192.168.1.5"), ("host", "192.168.1.5")]), &cors));
        assert!(origin_allowed(&headers(&[("origin", "http://printer:7125"), ("host", "printer:7125")]), &cors));
        assert!(!origin_allowed(&headers(&[("origin", "http://evil.example"), ("host", "192.168.1.5")]), &cors));
        assert!(origin_allowed(&headers(&[("origin", "https://app.fluidd.xyz"), ("host", "192.168.1.5")]), &cors));
        assert!(!origin_allowed(&headers(&[("origin", "null"), ("host", "192.168.1.5")]), &cors));
    }

    #[test]
    fn unix_users() {
        let a = auth("");
        let own = rustix::process::geteuid().as_raw();
        assert_eq!(a.unix_access(Some(own)).role, Some(Role::Admin));
        assert_eq!(a.unix_access(Some(0)).role, Some(Role::Admin));
        if own != 12345 {
            assert_eq!(a.unix_access(Some(12345)).role, None);
        }
    }

    #[test]
    fn file_roles() {
        let args = |v: Value| Args(v.as_object().unwrap().clone());
        let need = |rest, kind, a| files_need(rest, kind, &a);
        use serde_json::json;
        assert_eq!(need("list", RequestType::Get, args(json!({}))), Need::Role(Role::Viewer));
        assert_eq!(need("list", RequestType::Get, args(json!({"root": "config"}))), Need::Role(Role::Operator));
        assert_eq!(need("gcodes/a.gcode", RequestType::Get, args(json!({}))), Need::Role(Role::Viewer));
        assert_eq!(need("gcodes/a.gcode", RequestType::Delete, args(json!({}))), Need::Role(Role::Operator));
        assert_eq!(need("config/printer.cfg", RequestType::Delete, args(json!({}))), Need::Role(Role::Admin));
        // A path that climbs out of gcodes is judged by where it lands.
        assert_eq!(
            need("delete_file", RequestType::Delete, args(json!({"path": "gcodes/../config/orrery.toml"}))),
            Need::Role(Role::Admin)
        );
        assert_eq!(
            need("move", RequestType::Post, args(json!({"source": "gcodes/a", "dest": "config/a"}))),
            Need::Role(Role::Admin)
        );
        assert_eq!(
            need("copy", RequestType::Post, args(json!({"source": "config/a", "dest": "gcodes/a"}))),
            Need::Role(Role::Operator)
        );
        assert_eq!(need("zip", RequestType::Post, args(json!({"items": ["gcodes/a"]}))), Need::Role(Role::Admin));
        assert_eq!(
            need("zip", RequestType::Post, args(json!({"items": ["gcodes/a"], "dest": "gcodes/a.zip"}))),
            Need::Role(Role::Operator)
        );
        assert_eq!(need("upload", RequestType::Post, args(json!({"root": "config"}))), Need::Role(Role::Admin));
    }
}
