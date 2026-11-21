//! orrery's additions to Moonraker's agent API (see docs/design/2026-11-15-plugin-interface.md).
//!
//! - Any agent can register endpoints under its own prefix with
//!   `orrery.agent.register_endpoint`: `/agent/<name>/<path>` over HTTP, and
//!   `agent.<name>.<path>` over JSON-RPC. orrery passes each call on to the agent.
//! - An `[agent.<name>]` table in `orrery.toml` lets the printer's owner give an agent
//!   Moonraker's built-in names. `maps` points method prefixes and notifications at the agent,
//!   `component` lists a name in `server.info`, and `history_fields` lists the fields the agent
//!   may add to history jobs with `orrery.agent.history_field`.
//! - A configured agent's name can only be claimed over the unix socket, optionally by one Unix
//!   user, so another program can't connect under it and receive its calls. An agent with a
//!   `command` must connect from the process orrery started, or one that process started. An
//!   agent with `systemd_unit` must connect from a process in that unit, which orrery starts if
//!   it isn't running.
//! - Some agents are built in: a `server` option in `[spoolman]` turns on the Spoolman agent,
//!   with Moonraker's names, and starts `orrery-spoolman` from orrery's own folder. An
//!   `[agent.spoolman]` table overrides the built-in options one at a time.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::api::{Alias, ApiError, ApiResult, ConnId, Request, RequestType, Transports, handler};
use crate::auth::Role;
use crate::config::Config;
use crate::server::App;

/// One `[agent.<name>]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    /// The name listed in `server.info` components, such as `spoolman`.
    pub component: Option<String>,
    /// Method prefixes: `(public, agent)`, such as `("server.spoolman", "agent.spoolman")`.
    pub methods: Vec<(String, String)>,
    /// Notifications: `(public name without notify_, agent event)`.
    pub notifications: Vec<(String, String)>,
    pub history_fields: Vec<String>,
    /// The only Unix user that may connect as this agent.
    pub user: Option<u32>,
    pub command: Command,
    /// `systemd_unit`: the service the agent runs as.
    pub unit: Option<Unit>,
    /// `role`: what the agent may do beyond its own things. `agent`, the default, is viewer.
    pub role: Role,
    /// `call_role`: what a client needs to call the agent. The default is operator.
    pub call_role: Role,
}

/// A systemd service that runs an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// The unit's full name, such as `orrery-spoolman.service`.
    pub name: String,
    pub scope: Scope,
}

/// Which systemd runs a unit: the system's, or orrery's user's own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    System,
    User,
}

/// How an agent starts.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// No `command` option: something else starts the agent, unless it's built in.
    Unset,
    /// `command = false`: something else starts the agent, even a built-in one.
    Off,
    /// The program orrery starts for the agent, and its arguments.
    Run(Vec<String>),
}

#[derive(Debug, Default)]
pub struct Agents {
    configs: Vec<AgentConfig>,
    /// The process orrery started for each agent with a `command`, while it runs.
    processes: std::sync::Mutex<std::collections::HashMap<String, u32>>,
}

impl Agents {
    /// The agent tables, and warnings for `server.info`. `agent_dir` is where built-in agents'
    /// programs are: orrery's own folder.
    pub fn new(config: &Config, agent_dir: Option<&Path>) -> Result<(Self, Vec<String>)> {
        let mut configs = Vec::new();
        for section in config.section_names("agent ") {
            let name = section["agent ".len()..].to_owned();
            configs.push(parse_section(config, &section, &name)?);
        }
        let mut warnings = Vec::new();
        builtin_spoolman(config, agent_dir, &mut configs, &mut warnings);
        Ok((Self { configs, ..Self::default() }, warnings))
    }

    fn get(&self, name: &str) -> Option<&AgentConfig> {
        self.configs.iter().find(|c| c.name == name)
    }

    /// Whether `[agent.<name>]` exists, or a built-in agent of that name is on.
    pub fn is_configured(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The configured agent tables.
    pub fn configs(&self) -> &[AgentConfig] {
        &self.configs
    }

    /// What a client needs to call an agent: its `call_role`, or operator for an agent that
    /// isn't configured.
    pub fn call_role(&self, name: &str) -> Role {
        self.get(name).map_or(Role::Operator, |c| c.call_role)
    }

    /// The agent whose unit a service name, as clients see it, refers to: `orrery-spoolman`
    /// for a system unit, `user/orrery-spoolman` for a user unit.
    pub fn agent_of_service(&self, service: &str) -> Option<String> {
        self.units().find_map(|(name, unit)| {
            let short = unit.name.strip_suffix(".service").unwrap_or(&unit.name);
            let listed = match unit.scope {
                Scope::System => short.to_owned(),
                Scope::User => format!("user/{short}"),
            };
            (listed == service).then(|| name.to_owned())
        })
    }

    /// Records the process orrery started for an agent, or that it exited.
    pub fn set_process(&self, name: &str, pid: Option<u32>) {
        let mut processes = self.processes.lock().expect("processes lock");
        match pid {
            Some(pid) => processes.insert(name.to_owned(), pid),
            None => processes.remove(name),
        };
    }

    /// The agents that run as systemd units: `(name, unit)`.
    pub fn units(&self) -> impl Iterator<Item = (&str, &Unit)> {
        self.configs.iter().filter_map(|c| Some((c.name.as_str(), c.unit.as_ref()?)))
    }

    /// The agents orrery starts itself: `(name, command)`.
    pub fn commands(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.configs.iter().filter_map(|c| match &c.command {
            Command::Run(command) => Some((c.name.as_str(), command.as_slice())),
            _ => None,
        })
    }

    /// Component names configured agents add to `server.info`.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.configs.iter().filter_map(|c| c.component.as_deref())
    }

    /// The registry aliases for every configured method map.
    pub fn aliases(&self) -> Vec<Alias> {
        let mut out = Vec::new();
        for c in &self.configs {
            for (public, target) in &c.methods {
                out.push(Alias::new(public, target, &c.name));
            }
        }
        out
    }

    /// The public notification an agent's event is mapped to, without `notify_`.
    pub fn notification(&self, agent: &str, event: &str) -> Option<&str> {
        let c = self.get(agent)?;
        c.notifications.iter().find(|(_, e)| e == event).map(|(public, _)| public.as_str())
    }
}

fn parse_section(config: &Config, section: &str, name: &str) -> Result<AgentConfig> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("[{section}]: an agent name may use letters, digits, '_' and '-'");
    }
    let s = config.section(section);
    let component = s.get_opt_str("component");
    let history_fields = s.get_list("history_fields");
    let user = match s.get_value("user") {
        None => None,
        Some(Value::Number(n)) => Some(n.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(u32::MAX)),
        Some(Value::String(u)) => Some(user_id(&u).ok_or_else(|| anyhow::anyhow!("[{section}] user: no user {u:?}"))?),
        Some(other) => bail!("[{section}] user: expected a user name or id, got {other}"),
    };
    let command = match s.get_value("command") {
        None => Command::Unset,
        Some(Value::Bool(false)) => Command::Off,
        Some(Value::String(program)) if !program.is_empty() => Command::Run(vec![program]),
        Some(Value::Array(items)) if !items.is_empty() => {
            let words: Option<Vec<String>> = items.iter().map(|i| i.as_str().map(str::to_owned)).collect();
            Command::Run(words.filter(|w| !w[0].is_empty()).ok_or_else(|| {
                anyhow::anyhow!("[{section}] command: expected a list of strings, starting with the program")
            })?)
        }
        Some(other) => {
            bail!(
                "[{section}] command: expected the program, a list of the program and its arguments, or false, got {other}"
            )
        }
    };
    let unit = match s.get_opt_str("systemd_unit") {
        None => None,
        Some(unit) => {
            if unit.is_empty() || unit.contains('/') {
                bail!("[{section}] systemd_unit: expected a unit name, such as orrery-{name}.service, got {unit:?}");
            }
            let name = if unit.contains('.') { unit } else { format!("{unit}.service") };
            let scope = match s.get_str("systemd_scope", "system").as_str() {
                "system" => Scope::System,
                "user" => Scope::User,
                other => bail!("[{section}] systemd_scope: expected system or user, got {other:?}"),
            };
            Some(Unit { name, scope })
        }
    };
    if unit.is_some() && matches!(command, Command::Run(_)) {
        bail!("[{section}]: an agent has either command or systemd_unit, not both");
    }
    let mut methods = Vec::new();
    let mut notifications = Vec::new();
    let own = format!("agent.{name}");
    match s.get_value("maps") {
        None => {}
        Some(Value::Object(maps)) => {
            for (public, target) in maps {
                let Some(target) = target.as_str() else {
                    bail!("[{section}] maps: {public:?} must map to a string, got {target}");
                };
                if let Some(notify) = public.strip_prefix("notify_") {
                    let Some(event) = target.strip_prefix(&format!("{own}.")).filter(|e| !e.contains('.')) else {
                        bail!(
                            "[{section}] maps: {public} must map to one of this agent's events, such as {own}.<event>"
                        );
                    };
                    notifications.push((notify.to_owned(), event.to_owned()));
                    continue;
                }
                if target != own && !target.starts_with(&format!("{own}.")) {
                    bail!("[{section}] maps: {public} must map to this agent's methods, {own} or {own}.<prefix>");
                }
                if public.is_empty() || public.starts_with("agent.") || public.starts_with("orrery.") {
                    bail!("[{section}] maps: {public:?} can't be mapped; agent.* and orrery.* names are orrery's own");
                }
                methods.push((public, target.to_owned()));
            }
        }
        Some(other) => bail!("[{section}] maps: expected a table, got {other}"),
    }
    let role = match s.get_str("role", "agent").as_str() {
        "agent" => Role::Viewer,
        other => match Role::parse(other) {
            Some(r @ (Role::Operator | Role::Admin)) => r,
            _ => bail!("[{section}] role: expected \"agent\", \"operator\", or \"admin\", got {other:?}"),
        },
    };
    let call_role = s.get_str("call_role", "operator");
    let call_role = Role::parse(&call_role).ok_or_else(|| {
        anyhow::anyhow!("[{section}] call_role: expected \"viewer\", \"operator\", or \"admin\", got {call_role:?}")
    })?;
    Ok(AgentConfig {
        name: name.to_owned(),
        component,
        methods,
        notifications,
        history_fields,
        user,
        command,
        unit,
        role,
        call_role,
    })
}

/// The built-in Spoolman agent. It's off until `[spoolman]` has a `server`, in `moonraker.conf`
/// or `orrery.toml`, as Moonraker's component is. Then it gets Moonraker's names, and orrery
/// starts `orrery-spoolman` from `agent_dir`, unless `[agent.spoolman]` says otherwise.
fn builtin_spoolman(
    config: &Config,
    agent_dir: Option<&Path>,
    configs: &mut Vec<AgentConfig>,
    warnings: &mut Vec<String>,
) {
    let Some(options) = config.section_json("spoolman") else { return };
    if !options.contains_key("server") {
        warnings
            .push("[spoolman] has no server option, so Spoolman is off. Set it to the Spoolman server's URL.".into());
        return;
    }
    // In server.config, as Moonraker's component reads them: Fluidd connects to Spoolman itself,
    // at `server`, for live updates. A bad sync_rate is the agent's to report.
    let section = config.section("spoolman");
    section.get_str("server", "");
    let _ = section.get_int("sync_rate", 5);
    let c = match configs.iter().position(|c| c.name == "spoolman") {
        Some(i) => &mut configs[i],
        None => {
            configs.push(AgentConfig {
                name: "spoolman".into(),
                component: None,
                methods: Vec::new(),
                notifications: Vec::new(),
                history_fields: Vec::new(),
                user: None,
                command: Command::Unset,
                unit: None,
                role: Role::Viewer,
                call_role: Role::Operator,
            });
            configs.last_mut().expect("just pushed")
        }
    };
    if c.component.is_none() {
        c.component = Some("spoolman".into());
    }
    if c.history_fields.is_empty() {
        c.history_fields = vec!["spool_ids".into()];
    }
    if c.methods.is_empty() && c.notifications.is_empty() {
        c.methods = vec![("server.spoolman".into(), "agent.spoolman".into())];
        c.notifications = ["active_spool_set", "spoolman_status_changed"].map(|e| (e.to_owned(), e.to_owned())).into();
    }
    if c.command == Command::Unset && c.unit.is_none() {
        match agent_dir.map(|dir| dir.join("orrery-spoolman")) {
            Some(program) if program.is_file() => {
                c.command = Command::Run(vec![program.to_string_lossy().into_owned()])
            }
            program => {
                let place = program.map_or_else(|| "orrery's folder".to_owned(), |p| p.display().to_string());
                warnings.push(format!(
                    "Spoolman is set up in [spoolman], but orrery can't find its agent at {place}. Put \
                     orrery-spoolman there, or set command in [agent.spoolman] in orrery.toml."
                ));
            }
        }
    }
}

/// Looks up a user's id in `/etc/passwd`. A number is taken as the id itself.
fn user_id(user: &str) -> Option<u32> {
    if let Ok(id) = user.parse() {
        return Some(id);
    }
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.first() == Some(&user))?.get(2)?.parse().ok()
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register(
        "/orrery/agent/register_endpoint",
        &[RequestType::Post],
        Transports::WebsocketOnly,
        handler(register_endpoint),
    );
    r.register("/orrery/agent/history_field", &[RequestType::Post], Transports::WebsocketOnly, handler(history_field));
    r.register("/orrery/agent/config", &[RequestType::Get], Transports::WebsocketOnly, handler(agent_config));
}

/// Installs the configured maps, after every built-in endpoint is registered, and warns about
/// maps that replace orrery's own methods.
pub fn start(app: &Arc<App>) {
    let aliases = app.agents.aliases();
    let mut warnings = Vec::new();
    {
        let mut r = app.registry.write().expect("registry lock");
        for alias in &aliases {
            if r.has_prefix(&alias.public_method) {
                warnings.push(format!(
                    "orrery.toml maps {} to the agent {}, so the agent answers orrery's own {}.* methods.",
                    alias.public_method, alias.agent, alias.public_method
                ));
            }
        }
        r.set_aliases(aliases);
    }
    for w in warnings {
        app.add_warning(w);
    }
}

/// Checks that a connection may use an agent name. A connection that sent the agent's key may,
/// from anywhere. Otherwise a configured name needs the unix socket, the configured user when
/// there is one, and the process orrery started, or a process in the agent's unit. A connection
/// from another user than orrery's then gets the agent's rights.
pub async fn check_claim(app: &App, name: &str, conn: ConnId) -> Result<(), ApiError> {
    let Some(config) = app.agents.get(name) else { return Ok(()) };
    let access = app.client_access(conn);
    if access.agent.as_deref() == Some(name) {
        return Ok(());
    }
    check_process(app, name, config, conn).await?;
    if access.role.is_none() {
        let peer = app.client_peer(conn);
        let uid = peer.uid.map_or_else(|| "unknown".to_owned(), |u| u.to_string());
        let access = crate::auth::Access {
            role: Some(config.role),
            agent: Some(name.to_owned()),
            source: format!("agent {name} (unix socket, user {uid})"),
            ..crate::auth::Access::default()
        };
        tracing::info!(target: "orrery::auth", "Connection {conn} is agent {name}: {}", access.describe());
        app.set_client_access(conn, access);
    }
    Ok(())
}

/// The process checks for a configured agent name, without a key.
async fn check_process(app: &App, name: &str, config: &AgentConfig, conn: ConnId) -> Result<(), ApiError> {
    let peer = app.client_peer(conn);
    if !peer.unix {
        return Err(ApiError::bad_request(format!(
            "Agent '{name}' is configured in orrery.toml, so it must connect over the unix socket, or send its key"
        )));
    }
    if let Some(user) = config.user
        && peer.uid != Some(user)
    {
        return Err(ApiError::bad_request(format!("Agent '{name}' must connect as user {user}")));
    }
    if matches!(config.command, Command::Run(_)) {
        let started = app.agents.processes.lock().expect("processes lock").get(name).copied();
        if !started.zip(peer.pid).is_some_and(|(started, pid)| started_by(pid, started)) {
            return Err(ApiError::bad_request(format!(
                "Agent '{name}' is one orrery starts, so it must connect from the process orrery started"
            )));
        }
    }
    if let Some(unit) = &config.unit {
        let found = match peer.pid {
            Some(pid) => app.machine.services.unit_of_process(unit.scope, pid).await,
            None => Ok(None),
        };
        match found {
            Ok(Some(found)) if found == unit.name => {}
            Ok(_) => {
                return Err(ApiError::bad_request(format!("Agent '{name}' must connect from its unit, {}", unit.name)));
            }
            Err(e) => {
                return Err(ApiError::new(
                    503,
                    format!("orrery can't check that agent '{name}' runs in {}: {e}", unit.name),
                ));
            }
        }
    }
    Ok(())
}

/// Whether `pid` is the process `started`, or a process it started, such as the program a
/// wrapper script runs. Follows parent processes in `/proc`, at most 32 of them.
fn started_by(pid: u32, started: u32) -> bool {
    let mut pid = pid;
    for _ in 0..32 {
        if pid == started {
            return true;
        }
        match parent_of(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => return false,
        }
    }
    false
}

/// A process's parent, from `/proc/<pid>/stat`. The name there is in parentheses and may hold
/// spaces, so the fields are read after the last `)`.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    // The state, then the parent.
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Removes an agent's endpoints when it disconnects.
pub fn remove_agent(app: &App, name: &str) {
    app.registry.write().expect("registry lock").unregister_prefix(&format!("/agent/{name}/"));
}

/// The agent behind a request, or an error for a connection that isn't one.
fn require_agent(app: &App, req: &Request) -> Result<String, ApiError> {
    let conn = req.conn.ok_or_else(|| ApiError::bad_request("No connection detected"))?;
    let identity = app.client_identity(conn).unwrap_or_default();
    if identity["type"].as_str() != Some("agent") {
        return Err(ApiError::bad_request("Only connections of the 'agent' type can use orrery.agent methods"));
    }
    Ok(identity["name"].as_str().unwrap_or_default().to_owned())
}

async fn register_endpoint(app: Arc<App>, req: Request) -> ApiResult {
    let agent = require_agent(&app, &req)?;
    let path = req.args.str("path")?;
    let path = path.trim_matches('/');
    let valid = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !path.split('/').all(valid) {
        return Err(ApiError::bad_request(format!(
            "Invalid endpoint path {path:?}: use lowercase letters, digits, '_' and '/'"
        )));
    }
    let names = req.args.list("request_methods")?;
    let mut types = Vec::new();
    for n in &names {
        let t = RequestType::from_http(&n.to_ascii_uppercase())
            .ok_or_else(|| ApiError::bad_request(format!("Invalid request method {n:?}")))?;
        if !types.contains(&t) {
            types.push(t);
        }
    }
    if types.is_empty() {
        return Err(ApiError::bad_request("request_methods must list GET, POST, or DELETE"));
    }
    let full = format!("/agent/{agent}/{path}");
    let prefix = format!("agent.{agent}.");
    let mut r = app.registry.write().expect("registry lock");
    if r.by_path(&full).is_some() {
        return Err(ApiError::bad_request(format!("Endpoint {full} already registered")));
    }
    let name = agent.clone();
    let forward = handler(move |app: Arc<App>, req: Request| {
        let (agent, prefix) = (name.clone(), prefix.clone());
        async move {
            let method = app.registry.read().expect("registry lock").method_name(&req.endpoint, req.kind);
            let local = method.strip_prefix(&prefix).unwrap_or(&method).to_owned();
            crate::extensions::send_call(&app, &agent, &local, Value::Object(req.args.0)).await.map_err(endpoint_error)
        }
    });
    r.register(&full, &types, Transports::All, forward);
    tracing::info!("Agent {agent} registered endpoint {full}");
    Ok(json!("ok"))
}

/// An agent's error, answered from one of its endpoints. The agent's code is the HTTP status, as
/// a built-in endpoint's would be, so an agent that stands in for one, such as Spoolman's
/// `server.spoolman.proxy`, can answer with Moonraker's errors. A code that isn't an HTTP status,
/// such as JSON-RPC's -32602, becomes a 500 that keeps the agent's error as `data`.
fn endpoint_error(e: crate::extensions::CallError) -> ApiError {
    let error = match e {
        crate::extensions::CallError::Agent(error) => error,
        crate::extensions::CallError::Server(e) => return e,
    };
    let message = error["message"].as_str().unwrap_or("Agent error").to_owned();
    match error["code"].as_u64().and_then(|c| u16::try_from(c).ok()).filter(|c| (400..600).contains(c)) {
        Some(status) => {
            let e = ApiError::new(status, message);
            match error.get("data") {
                Some(data) => e.with_data(data.clone()),
                None => e,
            }
        }
        None => ApiError::new(500, message).with_data(error),
    }
}

async fn history_field(app: Arc<App>, req: Request) -> ApiResult {
    let agent = require_agent(&app, &req)?;
    let name = req.args.str("name")?;
    let allowed = app.agents.get(&agent).is_some_and(|c| c.history_fields.contains(&name));
    if !allowed {
        return Err(ApiError::bad_request(format!(
            "Agent {agent} may not set the history field {name}; list it in history_fields in orrery.toml"
        )));
    }
    let value = req.args.require("value")?.clone();
    let description = req.args.opt_str("description")?;
    let units = req.args.opt_str("units")?;
    crate::history::set_field(&app, &agent, &name, value, description, units).await?;
    Ok(json!("ok"))
}

/// An agent's own config: by default the section named after it, such as `[spoolman]`, from
/// `orrery.toml` with its types, or from `moonraker.conf` as text. An agent may read only its
/// own sections: `<name>`, `<name> <anything>`, and its `[agent.<name>]` table.
async fn agent_config(app: Arc<App>, req: Request) -> ApiResult {
    let agent = require_agent(&app, &req)?;
    let section = req.args.opt_str("section")?.unwrap_or_else(|| agent.clone());
    let own = section == agent || section.starts_with(&format!("{agent} ")) || section == format!("agent {agent}");
    if !own {
        return Err(ApiError::bad_request(format!("Agent {agent} may not read the section [{section}]")));
    }
    let options = app.config.section_json(&section);
    Ok(json!({"section": section, "exists": options.is_some(), "options": options.unwrap_or_default()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents(toml: &str) -> Result<Agents> {
        with_spoolman("[server]\n", toml, None).map(|(a, _)| a)
    }

    fn with_spoolman(conf: &str, toml: &str, dir: Option<&Path>) -> Result<(Agents, Vec<String>)> {
        let mut c = Config::parse(conf).unwrap();
        c.parse_toml(toml, Path::new("orrery.toml"))?;
        Agents::new(&c, dir)
    }

    #[test]
    fn spoolman_is_built_in() {
        let dir = tempfile::tempdir().unwrap();
        // Off without a server.
        let (a, w) = with_spoolman("[server]\n", "", Some(dir.path())).unwrap();
        assert!(a.get("spoolman").is_none() && w.is_empty());
        let (a, w) = with_spoolman("[spoolman]\nsync_rate: 5\n", "", Some(dir.path())).unwrap();
        assert!(a.get("spoolman").is_none());
        assert!(w[0].starts_with("[spoolman] has no server option"));

        // On with one, and a warning while the agent's program is missing.
        let conf = "[spoolman]\nserver: http://spoolman.local\n";
        let (a, w) = with_spoolman(conf, "", Some(dir.path())).unwrap();
        let c = a.get("spoolman").unwrap();
        assert_eq!(c.component.as_deref(), Some("spoolman"));
        assert_eq!(a.notification("spoolman", "spoolman_status_changed"), Some("spoolman_status_changed"));
        assert_eq!(c.command, Command::Unset);
        assert!(w[0].contains("can't find its agent at"), "{w:?}");

        let program = dir.path().join("orrery-spoolman");
        std::fs::write(&program, "").unwrap();
        let (a, w) = with_spoolman(conf, "", Some(dir.path())).unwrap();
        assert!(w.is_empty());
        let started: Vec<_> = a.commands().map(|(n, c)| (n.to_owned(), c.to_vec())).collect();
        assert_eq!(started, vec![("spoolman".to_owned(), vec![program.to_string_lossy().into_owned()])]);

        // [agent.spoolman] overrides one option at a time, and command = false starts nothing.
        let (a, w) = with_spoolman(conf, "[agent.spoolman]\ncommand = false\nuser = 1000\n", Some(dir.path())).unwrap();
        let c = a.get("spoolman").unwrap();
        assert!(w.is_empty());
        assert_eq!((c.command.clone(), c.user, c.component.as_deref()), (Command::Off, Some(1000), Some("spoolman")));
        assert_eq!(a.commands().count(), 0);
    }

    #[test]
    fn parses_tables() {
        let a = agents(
            r#"
[agent.spoolman]
component = "spoolman"
history_fields = ["spool_ids"]
user = 1000
command = ["~/bin/orrery-spoolman", "-v"]

[agent.spoolman.maps]
"server.spoolman" = "agent.spoolman"
"notify_active_spool_set" = "agent.spoolman.active_spool_set"
"#,
        )
        .unwrap();
        assert_eq!(
            a.get("spoolman"),
            Some(&AgentConfig {
                name: "spoolman".into(),
                component: Some("spoolman".into()),
                methods: vec![("server.spoolman".into(), "agent.spoolman".into())],
                notifications: vec![("active_spool_set".into(), "active_spool_set".into())],
                history_fields: vec!["spool_ids".into()],
                user: Some(1000),
                command: Command::Run(vec!["~/bin/orrery-spoolman".into(), "-v".into()]),
                unit: None,
                role: Role::Viewer,
                call_role: Role::Operator,
            })
        );
        assert_eq!(a.notification("spoolman", "active_spool_set"), Some("active_spool_set"));
        assert_eq!(a.components().collect::<Vec<_>>(), vec!["spoolman"]);
    }

    #[test]
    fn systemd_units() {
        let a = agents("[agent.a]\nsystemd_unit = \"orrery-a\"\n[agent.b]\nsystemd_unit = \"b.service\"\nsystemd_scope = \"user\"\n").unwrap();
        let units: Vec<_> = a.units().map(|(n, u)| (n, u.name.as_str(), u.scope)).collect();
        assert_eq!(units, [("a", "orrery-a.service", Scope::System), ("b", "b.service", Scope::User)]);
        let err = |t: &str| agents(t).unwrap_err().to_string();
        assert!(err("[agent.a]\nsystemd_unit = \"a\"\ncommand = \"x\"\n").contains("not both"));
        assert!(err("[agent.a]\nsystemd_unit = \"a\"\nsystemd_scope = \"session\"\n").contains("system or user"));
        assert!(err("[agent.a]\nsystemd_unit = \"/etc/a\"\n").contains("unit name"));
        // The built-in Spoolman agent doesn't start its own program when a unit runs it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orrery-spoolman"), "").unwrap();
        let conf = "[spoolman]\nserver: http://spoolman:7912\n";
        let (a, _) =
            with_spoolman(conf, "[agent.spoolman]\nsystemd_unit = \"orrery-spoolman\"\n", Some(dir.path())).unwrap();
        assert_eq!(a.commands().count(), 0);
        assert_eq!(a.units().count(), 1);
    }

    #[test]
    fn follows_parents() {
        let me = std::process::id();
        let parent = rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get() as u32);
        assert!(started_by(me, me));
        assert!(started_by(me, parent));
        let child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(started_by(child.id(), me));
        assert!(!started_by(me, child.id()));
        assert!(!started_by(u32::MAX, me));
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn rejects_maps_outside_the_agent() {
        let err = |t: &str| agents(t).unwrap_err().to_string();
        assert!(err("[agent.a.maps]\n\"server.x\" = \"agent.b\"\n").contains("this agent's methods"));
        assert!(err("[agent.a.maps]\n\"notify_x\" = \"agent.b.x\"\n").contains("this agent's events"));
        assert!(err("[agent.a.maps]\n\"orrery.x\" = \"agent.a\"\n").contains("orrery's own"));
        assert!(err("[agent.a]\nmaps = \"x\"\n").contains("expected a table"));
        assert!(err("[agent.a]\ncommand = [1]\n").contains("list of strings"));
        assert!(err("[agent.a]\ncommand = 5\n").contains("command"));
    }
}
