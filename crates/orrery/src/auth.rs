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

use std::collections::{HashMap, HashSet};
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
    /// Who the client is, for the log: its address as orrery judged it, such as
    /// "192.168.1.157", or "process 1234, user 1000" on the unix socket.
    pub client: String,
    /// The user logged in on this connection, or whose token the request carries.
    pub user: Option<String>,
    /// The role comes from the user's login: it's higher than the rest would give.
    pub from_login: bool,
    /// The user came from a trusted proxy's header, so they log out at the proxy.
    pub from_proxy: bool,
}

impl Access {
    pub fn none(source: impl Into<String>) -> Self {
        Self { source: source.into(), ..Self::default() }
    }

    pub fn admin(source: impl Into<String>) -> Self {
        Self { role: Some(Role::Admin), source: source.into(), ..Self::default() }
    }

    /// This access with a user logged in. A login adds rights and never takes any away, so the
    /// role is the higher of the two.
    pub fn with_user(&self, name: &str, role: Role) -> Self {
        self.with_login(name, role, "user", false)
    }

    /// This access with a user that a trusted proxy logged in. `access.info` reports it as
    /// trusted, as Fluidd needs to skip its login screen.
    pub fn with_proxy_user(&self, name: &str, role: Role) -> Self {
        let mut access = self.with_login(name, role, "proxy user", true);
        access.trusted = true;
        access
    }

    fn with_login(&self, name: &str, role: Role, kind: &str, from_proxy: bool) -> Self {
        let mut access = self.clone();
        access.user = Some(name.to_owned());
        access.from_proxy = from_proxy;
        if self.role.is_none_or(|r| role > r) {
            access.role = Some(role);
            access.from_login = true;
            access.agent = None;
            access.source = format!("{kind} {name}");
        } else {
            access.from_login = false;
            access.source = format!("{}, {kind} {name}", self.source);
        }
        access
    }

    /// Who asked, for log lines about what a client did: its address, which is often all
    /// orrery knows about a person, then its role and where that came from, such as
    /// "192.168.1.157 (admin, network 192.168.0.0/16)".
    pub fn requester(&self) -> String {
        let what = match (self.role, &self.agent) {
            (None, _) => format!("no rights, {}", self.source),
            (Some(role), Some(agent)) => format!("{role} as agent {agent}, {}", self.source),
            (Some(role), None) => format!("{role}, {}", self.source),
        };
        if self.client.is_empty() { what } else { format!("{} ({what})", self.client) }
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
    /// Calling a method of the agent of this name, named under `agent.<name>.`: the agent's
    /// `call_roles` or `call_role`, `operator` by default.
    Call(String, String),
}

/// Checks a request against what its endpoint needs. A refusal says what the request would
/// have done, who may do it, and what the client's role allows, without naming the network or
/// key that gave the role: a frontend shows the message to anyone at the screen.
pub fn authorize(app: &App, access: &Access, endpoint: &str, kind: RequestType, args: &Args) -> Result<(), ApiError> {
    let need = need(app, endpoint, kind, args);
    if need == Need::Anyone {
        return Ok(());
    }
    let Some(role) = access.role else {
        return Err(ApiError::new(401, "Unauthorized"));
    };
    let (needed, who) = match &need {
        Need::Anyone => return Ok(()),
        Need::Role(r) => (*r, format!("Only {}", plural(*r))),
        Need::Owner(agent) => {
            if access.agent.as_deref() == Some(agent.as_str()) {
                return Ok(());
            }
            (Role::Admin, format!("Only the agent {agent} or an admin"))
        }
        Need::Call(agent, method) => {
            let r = app.agents.call_role(agent, method);
            (r, format!("Only {}", plural(r)))
        }
    };
    if role >= needed {
        return Ok(());
    }
    let action = action(app, endpoint, kind, args);
    Err(ApiError::new(403, format!("{who} can {action}. {}", what_it_has(access, role))))
}

/// Arguments an audit line shows, when a request has them. Never a password or a token.
const AUDIT_ARGS: &[&str] = &[
    "root",
    "path",
    "source",
    "dest",
    "filename",
    "name",
    "username",
    "service",
    "namespace",
    "key",
    "agent",
    "script",
];

/// Logs one line for a request that changes something: who asked, what for, and whether it
/// failed (docs/design/2026-11-20-authorization.md, "Logging"). Requests that need `admin`, or
/// reach what an agent owns, log at info, except an agent's writes to its own data; the rest at
/// debug, since Fluidd writes its own state often and the log is on an SD card. Reading requests aren't logged, nor endpoints that need
/// nothing, such as logging in: those log their own outcome. Refusals are logged by
/// [`Auth::log_refusal`] instead.
pub fn audit(app: &App, access: &Access, endpoint: &str, kind: RequestType, args: &Args, outcome: Result<(), &str>) {
    if kind == RequestType::Get {
        return;
    }
    let important = match need(app, endpoint, kind, args) {
        Need::Anyone => return,
        Need::Role(role) => role == Role::Admin,
        // An agent's writes to its own data are its routine, such as the update agent saving
        // each entry's state after a check.
        Need::Owner(agent) => access.agent.as_deref() != Some(agent.as_str()),
        Need::Call(agent, method) => app.agents.call_role(&agent, &method) == Role::Admin,
    };
    let mut line = format!("{} asked to {}", access.requester(), action(app, endpoint, kind, args));
    let shown: Vec<String> = AUDIT_ARGS
        .iter()
        .filter_map(|k| {
            let v = args.get(k)?;
            let text = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
            let text = if text.chars().count() > 80 {
                format!("{}…", text.chars().take(80).collect::<String>())
            } else {
                text
            };
            Some(format!("{k} {text:?}"))
        })
        .collect();
    if !shown.is_empty() {
        line.push_str(&format!(" ({})", shown.join(", ")));
    }
    if let Err(e) = outcome {
        line.push_str(&format!(", which failed: {e}"));
    }
    if important {
        tracing::info!(target: "orrery::audit", "{line}");
    } else {
        tracing::debug!(target: "orrery::audit", "{line}");
    }
}

fn plural(role: Role) -> &'static str {
    match role {
        Role::Viewer => "a viewer, operator, or admin",
        Role::Operator => "an operator or admin",
        Role::Admin => "an admin",
    }
}

/// What the client has, and what that lets it do: "This connection has the viewer role, from
/// its network, so it can watch the printer but can't control it."
fn what_it_has(access: &Access, role: Role) -> String {
    let from = if access.from_login {
        "from its login".to_owned()
    } else if access.key.is_some() {
        "from its API key".to_owned()
    } else if access.trusted {
        "from its network".to_owned()
    } else if let Some(agent) = &access.agent {
        format!("as the agent {agent}")
    } else {
        "from the unix socket".to_owned()
    };
    let can = match (role, &access.agent) {
        (Role::Viewer, Some(_)) => "it can watch the printer and change only its own agent's data",
        (Role::Viewer, None) => "it can watch the printer but can't control it",
        (Role::Operator, _) => "it can print but can't change the printer's setup",
        (Role::Admin, _) => "it can do anything",
    };
    format!("This connection has the {role} role, {from}, so {can}.")
}

/// What a request would do, for a refusal: "reboot the printer's computer". Requests without a
/// description here are named by their method: "call server.history.delete_job".
fn action(app: &App, endpoint: &str, kind: RequestType, args: &Args) -> String {
    use RequestType::*;
    let arg = |k: &str| str_arg(args, k).unwrap_or_default();
    let folder = |path: &str| format!("the {} folder", root_of(path));
    if let Some(rest) = endpoint.strip_prefix("/server/files/") {
        let described = match (rest, kind) {
            ("list", _) => Some(format!("list {}", folder(&str_arg(args, "root").unwrap_or_else(|| "gcodes".into())))),
            ("upload", _) => {
                Some(format!("upload files to {}", folder(&str_arg(args, "root").unwrap_or_else(|| "gcodes".into()))))
            }
            ("directory", Get) => Some(format!("list {}", folder(&arg("path")))),
            ("directory", Post) => Some(format!("make folders in {}", folder(&arg("path")))),
            ("directory", Delete) => Some(format!("delete folders in {}", folder(&arg("path")))),
            ("delete_file", _) => Some(format!("delete files in {}", folder(&arg("path")))),
            ("move", _) => Some(format!("move files from {} to {}", folder(&arg("source")), folder(&arg("dest")))),
            ("copy", _) => Some(format!("copy files from {} to {}", folder(&arg("source")), folder(&arg("dest")))),
            ("zip", _) => Some("make zip files".to_owned()),
            ("metascan", _) => Some("scan files for their metadata".to_owned()),
            ("klippy.log" | "moonraker.log", _) => Some("download logs".to_owned()),
            (_, _) if !rest.contains('/') && !rest.contains('.') => None,
            (path, Delete) => Some(format!("delete files in {}", folder(path))),
            (path, _) => Some(format!("download files from {}", folder(path))),
        };
        if let Some(d) = described {
            return d;
        }
    }
    if endpoint.starts_with("/agent/") || endpoint == "/server/extensions/request" {
        let (agent, method) = match endpoint.strip_prefix("/agent/") {
            Some(rest) => {
                let (agent, method) = rest.split_once('/').unwrap_or((rest, ""));
                (agent.to_owned(), method.replace('/', "."))
            }
            None => (arg("agent"), arg("method")),
        };
        // When the agent's methods have roles of their own, the method is what was refused.
        if !method.is_empty() && app.agents.has_call_roles(&agent) {
            return format!("call {method} on the agent {agent}");
        }
        return format!("use the agent {agent}");
    }
    if let Some(verb) = endpoint.strip_prefix("/machine/services/") {
        return format!("{verb} the {} service", arg("service"));
    }
    let described = match (endpoint, kind) {
        ("/server/database/item", Get) => Some(format!("read the database namespace {}", arg("namespace"))),
        ("/server/database/item", _) => Some(format!("change the database namespace {}", arg("namespace"))),
        ("/machine/reboot", _) => Some("reboot the printer's computer".to_owned()),
        ("/machine/shutdown", _) => Some("shut down the printer's computer".to_owned()),
        ("/server/restart", _) => Some("restart orrery".to_owned()),
        ("/printer/restart" | "gcode/restart", _) => Some("restart Klipper".to_owned()),
        ("/printer/firmware_restart" | "gcode/firmware_restart", _) => Some("restart Klipper's firmware".to_owned()),
        ("/printer/emergency_stop" | "emergency_stop", _) => Some("use the emergency stop".to_owned()),
        ("/printer/gcode/script" | "gcode/script", _) => Some("send gcode".to_owned()),
        ("/printer/print/start", _) => Some("start a print".to_owned()),
        ("/printer/print/pause" | "pause_resume/pause", _) => Some("pause the print".to_owned()),
        ("/printer/print/resume" | "pause_resume/resume", _) => Some("resume the print".to_owned()),
        ("/printer/print/cancel" | "pause_resume/cancel", _) => Some("cancel the print".to_owned()),
        ("/server/history/job", Delete) => Some("delete print history".to_owned()),
        ("/server/history/reset_totals", _) => Some("reset the print history's totals".to_owned()),
        ("/server/webcams/item", _) => Some("change webcams".to_owned()),
        ("/server/logs/rollover", _) => Some("roll over the logs".to_owned()),
        ("/access/api_key", Get) => Some("see the API key".to_owned()),
        ("/access/api_key", _) => Some("make a new API key".to_owned()),
        ("/orrery/access/key/get", _) => Some("see API keys".to_owned()),
        ("/orrery/access/key/list", _) => Some("list API keys".to_owned()),
        ("/orrery/access/user/list", _) => Some("list users".to_owned()),
        ("/orrery/access/key/rotate", _) => Some("rotate API keys".to_owned()),
        ("/access/users/list", _) => Some("list users".to_owned()),
        ("/access/user", Post) => Some("add users".to_owned()),
        ("/access/user", Delete) => Some("delete users".to_owned()),
        ("/orrery/access/user/password", _) => Some("set other users' passwords".to_owned()),
        (e, Get) if e.starts_with("/server/job_queue/") => None,
        (e, _) if e.starts_with("/server/job_queue/") => Some("change the job queue".to_owned()),
        _ => None,
    };
    match described {
        Some(d) => d,
        None => format!("call {}", app.registry.read().expect("registry lock").method_name(endpoint, kind)),
    }
}

/// What an endpoint needs. `endpoint` is the path as registered: `/server/info`, or a Klippy
/// endpoint such as `gcode/script`, which has no leading slash.
fn need(app: &App, endpoint: &str, kind: RequestType, args: &Args) -> Need {
    use Role::*;
    if !endpoint.starts_with('/') {
        return klippy_need(endpoint);
    }
    if let Some(rest) = endpoint.strip_prefix("/agent/") {
        let (agent, method) = rest.split_once('/').unwrap_or((rest, ""));
        return Need::Call(agent.to_owned(), method.to_owned());
    }
    if let Some(rest) = endpoint.strip_prefix("/server/files/") {
        return files_need(rest, kind, args);
    }
    let get = kind == RequestType::Get;
    let role = match endpoint {
        // Logging in needs no rights, and the rest check for a user themselves.
        "/access/info"
        | "/server/connection/identify"
        | "/access/login"
        | "/access/refresh_jwt"
        | "/access/logout"
        | "/access/user/password" => return Need::Anyone,
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
            let arg = |k| args.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            return Need::Call(arg("agent"), arg("method"));
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
    /// `[authorization.proxy_login] header`, as written: the header that names a user the
    /// proxy logged in. Looking it up ignores case.
    proxy_login: Option<String>,
    /// The proxies came from `trusted_proxies`, not from `trusted_clients`.
    explicit_proxies: bool,
    use_xheaders: bool,
    /// orrery's own user id, which is admin on the unix socket.
    own_uid: u32,
    /// What has been logged once already, so repeats don't fill the log.
    logged: Mutex<HashSet<String>>,
    /// Refusals since the last hourly total, and when that total was logged.
    refusals: Mutex<(u64, Instant)>,
    /// `force_logins`: networks give no role, so browsers have to log in.
    force_logins: bool,
    /// One-shot tokens: the rights of the connection that asked, and when each expires.
    oneshots: Mutex<HashMap<String, Oneshot>>,
}

/// A one-shot token's rights, for one request from the same address.
struct Oneshot {
    access: Access,
    expires: Instant,
}

/// The header a proxy names its user in, as oauth2-proxy sends it.
const DEFAULT_PROXY_HEADER: &str = "X-Forwarded-User";

/// How long a one-shot token lasts, as in Moonraker (`ONESHOT_TIMEOUT`).
const ONESHOT_LIFETIME: std::time::Duration = std::time::Duration::from_secs(5);

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
        let proxy_login = match config.section_json("authorization proxy_login") {
            None => None,
            Some(table) => {
                let header = match table.get("header") {
                    None => DEFAULT_PROXY_HEADER.to_owned(),
                    Some(serde_json::Value::String(h)) => h.trim().to_owned(),
                    Some(other) => bail!("[authorization.proxy_login] header: {other} isn't a header name"),
                };
                if axum::http::HeaderName::from_bytes(header.as_bytes()).is_err() {
                    bail!(
                        "[authorization.proxy_login] header: {header:?} isn't a header name, such as \"X-Forwarded-User\""
                    );
                }
                if !explicit_proxies {
                    bail!(
                        "[authorization.proxy_login] needs trusted_proxies in [authorization]: orrery reads {header} only from the proxies it lists"
                    );
                }
                Some(header)
            }
        };
        let use_xheaders = config.section("server").get_bool("use_xheaders", true)?;
        let own_uid = rustix::process::geteuid().as_raw();
        Ok((
            Self {
                trusted,
                networks_from,
                ignored_trusted_clients,
                proxies,
                proxy_login,
                explicit_proxies,
                use_xheaders,
                own_uid,
                logged: Mutex::new(HashSet::new()),
                refusals: Mutex::new((0, Instant::now())),
                force_logins: section.get_bool("force_logins", false)?,
                oneshots: Mutex::new(HashMap::new()),
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
        if self.trusted.is_empty() && self.ignored_trusted_clients {
            tracing::warn!(target: "orrery::auth", "Authorization: [authorization.networks] lists no networks, and it replaces trusted_clients, so every client needs a key. To keep trusted_clients, remove the [authorization.networks] table.");
        } else if self.trusted.is_empty() {
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
            if let Some(header) = &self.proxy_login {
                tracing::info!(target: "orrery::auth", "Logins: trusted proxies name the user they logged in with {header}");
            }
            if self.explicit_proxies {
                self.warn_proxies_with_roles();
            }
        }
    }

    /// Warns about each trusted proxy that also has a network's role. Its requests without the
    /// headers get that role: from programs on the proxy's host that's often wanted, as on a
    /// printer where the proxy and local services share 127.0.0.1. But a proxy that drops the
    /// headers would hand the role to everyone it serves.
    fn warn_proxies_with_roles(&self) {
        for proxy in &self.proxies {
            let access = self.network_access(proxy.addr, true);
            let Some(role) = access.role else { continue };
            tracing::warn!(
                target: "orrery::auth",
                "Authorization: trusted proxy {proxy} has the {role} role, from {}, so its requests \
                 without X-Real-Ip or X-Forwarded-For get {role}. If the proxy drops those headers, \
                 everyone it serves gets {role}.",
                access.source
            );
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
        let who = if access.client.is_empty() {
            access.source.clone()
        } else {
            format!("{} ({})", access.client, access.source)
        };
        let first = self.logged.lock().expect("logged lock").insert(format!("refused {who} {}", error.message));
        if first {
            // The client is told only the kind of source; the log names it, for the admin.
            let message = error.message.trim_end_matches('.');
            tracing::warn!(target: "orrery::auth", "Refused {method} from {who}: {message}. Later refusals like it are only counted.");
            return;
        }
        let mut refusals = self.refusals.lock().expect("refusals lock");
        refusals.0 += 1;
        if refusals.1.elapsed() >= REFUSAL_SUMMARY {
            tracing::warn!(target: "orrery::auth", "Refused {} more requests in the last hour", refusals.0);
            *refusals = (0, Instant::now());
        }
    }

    /// The user a trusted proxy logged in, from `[authorization.proxy_login] header`. From any
    /// other address the header is ignored, since anyone can send it.
    pub fn proxy_user(&self, peer: IpAddr, headers: &HeaderMap) -> Option<String> {
        let header = self.proxy_login.as_deref()?;
        let name = headers.get(header).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())?;
        let peer = peer.to_canonical();
        if !self.is_proxy(peer) {
            self.log_once(format!("untrusted-login {peer}"), || {
                format!("{peer} sent {header}, but it isn't a trusted proxy, so orrery ignores the user it names")
            });
            return None;
        }
        Some(name.to_owned())
    }

    /// Whether this is the first time `key` is seen, for log lines written once.
    pub fn first_time(&self, key: String) -> bool {
        self.logged.lock().expect("logged lock").insert(key)
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
                    format!("Trusted proxy {peer} sent a request without X-Real-Ip or X-Forwarded-For, so orrery judges it by {peer} itself. That's expected from programs on the proxy's host.")
                });
            }
            return peer;
        };
        if !self.is_proxy(peer) {
            if self.use_xheaders {
                self.log_once(format!("untrusted-headers {peer}"), || {
                    format!("{peer} sent proxy headers, but it isn't a trusted proxy, so orrery ignores them and judges the request by {peer}. If {peer} is your proxy, list it in trusted_proxies.")
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
        // The most specific network decides, as in a routing table, so a single address can get
        // less than the network around it, or more. No two entries are the same network.
        let best = self.trusted.iter().filter(|(n, _)| n.contains(client)).max_by_key(|(n, r)| (n.len, *r));
        let Some((network, role)) = best else {
            return Access::none(format!("{client} isn't in a network with a role"));
        };
        if !origin_ok {
            return Access::none(format!("{client} is in network {network}, but the request's Origin isn't allowed"));
        }
        if self.force_logins {
            return Access::none(format!("{client} is in network {network}, but force_logins is on"));
        }
        Access { role: Some(*role), trusted: true, source: format!("network {network}"), ..Access::default() }
    }

    /// Keeps a one-shot token with the rights of the connection that asked for it.
    pub fn issue_oneshot(&self, token: String, access: Access) {
        let now = Instant::now();
        let mut oneshots = self.oneshots.lock().expect("one-shot lock");
        oneshots.retain(|_, o| o.expires > now);
        oneshots.insert(token, Oneshot { access, expires: now + ONESHOT_LIFETIME });
    }

    /// A one-shot token's rights, used up. `None` for a token that isn't one, has expired, or
    /// was issued to another address.
    pub fn use_oneshot(&self, token: &str, client: &str) -> Option<Access> {
        let oneshot = self.oneshots.lock().expect("one-shot lock").remove(token)?;
        if oneshot.expires <= Instant::now() {
            tracing::debug!(target: "orrery::auth", "A one-shot token from {client} had expired");
            return None;
        }
        if oneshot.access.client != client {
            tracing::info!(
                target: "orrery::auth",
                "Refused a one-shot token from {client}: it was issued to {}",
                oneshot.access.client
            );
            return None;
        }
        Some(oneshot.access)
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

    #[test]
    fn requesters_name_the_address() {
        let a = auth("[authorization]\ntrusted_clients:\n  192.168.0.0/16\n");
        let mut access = a.network_access(ip("192.168.1.157"), true);
        access.client = "192.168.1.157".into();
        assert_eq!(access.requester(), "192.168.1.157 (admin, network 192.168.0.0/16)");
        let user = access.with_user("sami", Role::Admin);
        assert_eq!(user.requester(), "192.168.1.157 (admin, network 192.168.0.0/16, user sami)");
    }

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
    fn proxy_users_come_only_from_trusted_proxies() {
        let a =
            toml_auth("[authorization]\ntrusted_proxies = [\"192.168.1.10\"]\n[authorization.proxy_login]\n").unwrap();
        let h = headers(&[("x-forwarded-user", "sami")]);
        assert_eq!(a.proxy_user(ip("192.168.1.10"), &h).as_deref(), Some("sami"));
        // Anyone else could claim any name.
        assert_eq!(a.proxy_user(ip("192.168.1.11"), &h), None);
        assert_eq!(a.proxy_user(ip("192.168.1.10"), &headers(&[("x-forwarded-user", " ")])), None);

        let a = toml_auth(
            "[authorization]\ntrusted_proxies = [\"192.168.1.10\"]\n[authorization.proxy_login]\nheader = \"Remote-User\"\n",
        )
        .unwrap();
        assert_eq!(a.proxy_user(ip("192.168.1.10"), &h), None);
        assert_eq!(a.proxy_user(ip("192.168.1.10"), &headers(&[("remote-user", "sami")])).as_deref(), Some("sami"));
        // A user named by the proxy is trusted, as Fluidd needs, and logs out at the proxy.
        let user = Access::none("network").with_proxy_user("sami", Role::Operator);
        assert!(user.trusted && user.from_proxy);
        assert_eq!(user.source, "proxy user sami");
        assert!(!user.with_user("olive", Role::Admin).from_proxy);

        for toml in [
            // Without trusted_proxies, orrery can't tell who may send the header.
            "[authorization.proxy_login]\n",
            "[authorization]\ntrusted_proxies = [\"192.168.1.10\"]\n[authorization.proxy_login]\nheader = \"not a header\"\n",
            "[authorization]\ntrusted_proxies = [\"192.168.1.10\"]\n[authorization.proxy_login]\nheader = 1\n",
        ] {
            assert!(toml_auth(toml).is_err(), "{toml}");
        }
    }

    #[test]
    fn networks_and_proxies() {
        // With [authorization.networks], no proxy is trusted unless trusted_proxies says so.
        let a = toml_auth("[authorization.networks]\nadmin = [\"127.0.0.0/8\"]\n").unwrap();
        let h = headers(&[("x-real-ip", "10.1.1.1")]);
        assert_eq!(a.client_address(ip("127.0.0.1"), &h), ip("127.0.0.1"));

        // A proxy inside a network gets the network's role; its users are judged by the headers.
        let toml = "[authorization]\ntrusted_proxies = [\"192.168.1.10\"]\n\
                    [authorization.networks]\noperator = [\"192.168.1.0/24\"]\n";
        let a = toml_auth(toml).unwrap();
        assert_eq!(a.network_access(ip("192.168.1.10"), true).role, Some(Role::Operator));
        assert_eq!(a.network_access(ip("192.168.1.11"), true).role, Some(Role::Operator));
        let h = headers(&[("x-real-ip", "192.168.1.20")]);
        let client = a.client_address(ip("192.168.1.10"), &h);
        assert_eq!(a.network_access(client, true).role, Some(Role::Operator));

        // So may a network entry inside a proxy.
        let toml = "[authorization]\ntrusted_proxies = [\"127.0.0.0/8\"]\n\
                    [authorization.networks]\nadmin = [\"127.0.0.1\"]\n";
        let a = toml_auth(toml).unwrap();
        assert_eq!(a.network_access(ip("127.0.0.1"), true).role, Some(Role::Admin));
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
