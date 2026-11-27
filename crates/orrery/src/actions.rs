//! Actions: named steps that buttons, Klipper's macros, and clients run
//! (docs/design/2026-11-23-buttons-and-actions.md).
//!
//! - Orrery's own actions are `[action.<name>]` in `orrery.toml`, and call Orrery's methods.
//!   An agent's are `[agent.<agent>.action.<name>]`, and call that agent's methods.
//! - An action is named by its table's path, `action.<name>` or
//!   `agent.<agent>.action.<name>`, or by the short forms `<name>` and `<agent>.<name>`.
//! - Some are built in, such as `shutdown`; a table of the same name replaces one.
//! - Orrery runs them: `server.actions.run` for clients and agents, the remote method
//!   `run_action` for Klipper, and Moonraker's remote methods under `[moonraker_compat]`.
//! - A caller needs the action's `role`, `operator` by default, and each call is checked
//!   against the caller's own role, so an action never does more than its caller could.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::agents::Agents;
use crate::api::{ApiError, ApiResult, Registry, Request, RequestType, Transports, handler};
use crate::auth::{Access, Role};
use crate::config::Config;
use crate::server::App;

/// Klipper's remote method for running an action from a macro.
pub const RUN_ACTION: &str = "run_action";

/// Moonraker's remote methods, as `[moonraker_compat]` registers them: the action each one runs,
/// and whether it runs as `admin` from Klipper with `klipper_shutdown`.
const MOONRAKER_REMOTE_METHODS: &[(&str, &str, bool)] = &[
    ("shutdown_machine", "shutdown", true),
    ("reboot_machine", "reboot", true),
    ("spoolman_set_active_spool", "spoolman.set_active_spool", false),
    ("pause_job_queue", "job_queue_pause", false),
    ("start_job_queue", "job_queue_start", false),
];

/// Orrery's built-in actions: name and method. Each has the default role.
const BUILT_IN: &[(&str, &str)] = &[
    ("shutdown", "machine.shutdown"),
    ("reboot", "machine.reboot"),
    ("emergency_stop", "printer.emergency_stop"),
    ("job_queue_pause", "server.job_queue.pause"),
    ("job_queue_start", "server.job_queue.start"),
];

/// The built-in action that calls `method` with no arguments.
pub fn built_in_for(method: &str) -> Option<&'static str> {
    BUILT_IN.iter().find(|(_, m)| *m == method).map(|(name, _)| *name)
}

/// Built-in agents' actions: agent, name, and the agent's method.
const BUILT_IN_AGENT: &[(&str, &str, &str)] = &[("spoolman", "set_active_spool", "post_spool_id")];

/// What an action does.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Method { method: String, params: Map<String, Value> },
    Gcode(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    /// The agent whose action it is, or `None` for Orrery's own.
    pub agent: Option<String>,
    pub name: String,
    pub step: Step,
    /// What a caller needs to run it.
    pub role: Role,
    /// Where an action that has no table came from, for warnings, such as a button's template
    /// in moonraker.conf.
    pub from: Option<String>,
}

impl Action {
    /// The short name, as the docs write it: `save_state`, or `power.printer_toggle`.
    pub fn short_name(&self) -> String {
        match &self.agent {
            Some(agent) => format!("{agent}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// The JSON-RPC method the action calls.
    fn method(&self) -> (String, Map<String, Value>) {
        match (&self.step, &self.agent) {
            (Step::Method { method, params }, Some(agent)) => (format!("agent.{agent}.{method}"), params.clone()),
            (Step::Method { method, params }, None) => (method.clone(), params.clone()),
            (Step::Gcode(script), _) => {
                ("printer.gcode.script".to_owned(), Map::from_iter([("script".to_owned(), json!(script))]))
            }
        }
    }
}

/// Splits an action's name, full or short, into its agent and its name. `None` for a name that
/// doesn't read as one.
pub fn parse_name(name: &str) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = name.split('.').collect();
    let (agent, action) = match parts.as_slice() {
        ["action", action] => (None, *action),
        ["agent", agent, "action", action] => (Some(*agent), *action),
        [agent, action] if !matches!(*agent, "action" | "agent") => (Some(*agent), *action),
        [action] => (None, *action),
        _ => return None,
    };
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    (valid(action) && agent.is_none_or(valid)).then_some((agent, action))
}

#[derive(Debug, Default)]
pub struct Actions {
    actions: Vec<Action>,
    /// Orrery's actions the startup check left out, by short name, with why.
    left_out: Mutex<Vec<(String, String)>>,
}

impl Actions {
    /// The actions in the config, with the built-in ones, and warnings for tables that can't be
    /// used.
    /// `converted` are actions made from elsewhere, such as buttons' moonraker.conf templates;
    /// a table of the same name replaces one.
    pub fn new(config: &Config, agents: &Agents, converted: Vec<Action>) -> (Self, Vec<String>) {
        let mut actions = Vec::new();
        let mut warnings = Vec::new();
        for section in config.section_names("action ") {
            let name = &section["action ".len()..];
            let options = config.section_json(&section).unwrap_or_default();
            match parse_action(None, name, &options) {
                Ok(a) => actions.push(a),
                Err(e) => warnings.push(format!("[action.{name}]: {e}")),
            }
        }
        for c in agents.configs() {
            let Some(tables) = config.section_json(&format!("agent {}", c.name)).and_then(|o| o.get("action").cloned())
            else {
                continue;
            };
            let Value::Object(tables) = tables else {
                warnings.push(format!(
                    "[agent.{}] action: expected tables, such as [agent.{}.action.<name>]",
                    c.name, c.name
                ));
                continue;
            };
            for (name, options) in tables {
                let where_ = format!("[agent.{}.action.{name}]", c.name);
                let Value::Object(options) = options else {
                    warnings.push(format!("{where_}: expected a table"));
                    continue;
                };
                match parse_action(Some(&c.name), &name, &options) {
                    Ok(a) => actions.push(a),
                    Err(e) => warnings.push(format!("{where_}: {e}")),
                }
            }
        }
        for a in converted {
            if !actions.iter().any(|t: &Action| t.agent.is_none() && t.name == a.name) {
                actions.push(a);
            }
        }
        for (name, method) in BUILT_IN {
            if !actions.iter().any(|a| a.agent.is_none() && a.name == *name) {
                actions.push(Action {
                    agent: None,
                    name: (*name).to_owned(),
                    step: Step::Method { method: (*method).to_owned(), params: Map::new() },
                    role: Role::Operator,
                    from: None,
                });
            }
        }
        for (agent, name, method) in BUILT_IN_AGENT {
            let taken = actions.iter().any(|a| a.agent.as_deref() == Some(*agent) && a.name == *name);
            if agents.is_configured(agent) && !taken {
                actions.push(Action {
                    agent: Some((*agent).to_owned()),
                    name: (*name).to_owned(),
                    step: Step::Method { method: (*method).to_owned(), params: Map::new() },
                    role: Role::Operator,
                    from: None,
                });
            }
        }
        (Self { actions, left_out: Mutex::default() }, warnings)
    }

    /// The action of this name, full or short.
    pub fn get(&self, name: &str) -> Option<&Action> {
        let (agent, action) = parse_name(name)?;
        let found = self.actions.iter().find(|a| a.agent.as_deref() == agent && a.name == action)?;
        let left_out = self.left_out.lock().expect("actions lock");
        (!left_out.iter().any(|(n, _)| *n == found.short_name())).then_some(found)
    }

    /// Checks Orrery's actions against its methods, once they're registered, and leaves out
    /// those that can't work. Klippy's `printer.*` methods that Orrery doesn't know come when
    /// Klippy connects, so they aren't checked. Returns the warnings.
    pub fn check(&self, registry: &Registry) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut left_out = self.left_out.lock().expect("actions lock");
        for a in self.actions.iter().filter(|a| a.agent.is_none()) {
            let Step::Method { method, .. } = &a.step else { continue };
            let why = if let Some(agent) = registry.alias_agent(method) {
                format!(
                    "{method} belongs to the agent {agent}; declare the action in its table, as [agent.{agent}.action.{}]",
                    a.name
                )
            } else if registry.by_method(method).is_none() && !method.starts_with("printer.") {
                format!("Orrery has no method {method}")
            } else {
                continue;
            };
            warnings.push(match &a.from {
                Some(from) => format!("{from}: {why}, so it's left out; set this button's actions in orrery.toml"),
                None => format!("[action.{}]: {why}, so the action is left out", a.name),
            });
            left_out.push((a.short_name(), why));
        }
        warnings
    }

    /// Warnings for connected agents' actions whose methods the agent hasn't registered.
    pub fn agent_warnings(&self, registry: &Registry, connected: impl Fn(&str) -> bool) -> Vec<String> {
        let mut out = Vec::new();
        for a in &self.actions {
            let (Some(agent), Step::Method { method, .. }) = (&a.agent, &a.step) else { continue };
            if connected(agent) && registry.by_method(&format!("agent.{agent}.{method}")).is_none() {
                out.push(format!("[agent.{agent}.action.{}]: the agent {agent} has no method {method}", a.name));
            }
        }
        out
    }
}

fn parse_action(agent: Option<&str>, name: &str, options: &Map<String, Value>) -> Result<Action, String> {
    if parse_name(name).is_none_or(|(a, n)| a.is_some() || n != name) {
        return Err("an action name may use letters, digits, '_' and '-'".into());
    }
    for key in options.keys() {
        if !matches!(key.as_str(), "method" | "params" | "gcode" | "role") {
            return Err(format!("unknown option {key}; an action has method, params, gcode, and role"));
        }
    }
    let text = |key: &str| match options.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(other) => Err(format!("{key}: expected text, got {other}")),
    };
    let params = match options.get("params") {
        None => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(other) => return Err(format!("params: expected a table, got {other}")),
    };
    let step = match (text("method")?, text("gcode")?) {
        (Some(method), None) => Step::Method { method, params },
        (None, Some(gcode)) if params.is_empty() => Step::Gcode(gcode),
        (None, Some(_)) => return Err("a gcode action has no params".into()),
        (Some(_), Some(_)) => return Err("an action has method or gcode, not both".into()),
        (None, None) => return Err("an action needs method or gcode".into()),
    };
    if agent.is_some() && matches!(step, Step::Method { ref method, .. } if method.contains('.')) {
        return Err("method: name one of the agent's own methods, such as post_spool_id, without agent.<name>.".into());
    }
    let role = match options.get("role") {
        None => Role::Operator,
        Some(Value::String(r)) => {
            Role::parse(r).ok_or_else(|| format!("role: expected viewer, operator, or admin, got {r:?}"))?
        }
        Some(other) => return Err(format!("role: expected viewer, operator, or admin, got {other}")),
    };
    Ok(Action { agent: agent.map(str::to_owned), name: name.to_owned(), step, role, from: None })
}

/// Runs one action for `access`, with the caller's arguments beside the action's own `params`.
pub async fn run(app: &Arc<App>, access: &Access, name: &str, args: Map<String, Value>) -> ApiResult {
    let Some(action) = app.actions.get(name).cloned() else {
        return Err(ApiError::bad_request(format!("No action {name}")));
    };
    let short = action.short_name();
    let role = access.role.ok_or_else(|| ApiError::new(401, "Unauthorized"))?;
    if role < action.role {
        return Err(crate::auth::refusal(access, role, action.role, &format!("run the action {short}")));
    }
    let (method, mut params) = action.method();
    if matches!(action.step, Step::Gcode(_)) && !args.is_empty() {
        return Err(ApiError::bad_request(format!("The action {short} runs gcode, and takes no arguments")));
    }
    for (key, value) in args {
        if params.contains_key(&key) {
            return Err(ApiError::bad_request(format!("The action {short} sets {key} itself")));
        }
        params.insert(key, value);
    }
    let result = app.call_method(access, &method, params).await;
    match &result {
        Ok(_) => tracing::debug!("Action {short} ran, as {} asked", access.requester()),
        Err(e) => tracing::info!("Action {short} failed, as {} asked: {e}", access.requester()),
    }
    result
}

/// `server.actions.run {action, ...}`: the other arguments go to the action's method.
async fn run_request(app: Arc<App>, req: Request) -> ApiResult {
    let name = req.args.str("action")?;
    let mut args = req.args.0.clone();
    args.remove("action");
    run(&app, &req.access, &name, args).await
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register("/server/actions/run", &[RequestType::Post], Transports::All, handler(run_request));
}

/// The remote methods Orrery registers with Klipper: `run_action`, and Moonraker's whose
/// actions exist, unless `[moonraker_compat]` turns them off.
pub fn remote_methods(app: &App) -> Vec<&'static str> {
    let mut out = vec![RUN_ACTION];
    if app.compat.remote_methods {
        out.extend(
            MOONRAKER_REMOTE_METHODS.iter().filter(|(_, action, _)| app.actions.get(action).is_some()).map(|m| m.0),
        );
    }
    out
}

/// Whether Klipper's remote method `method` is one of the ones [`remote_methods`] registers.
pub fn is_remote_method(app: &App, method: &str) -> bool {
    remote_methods(app).contains(&method)
}

/// Runs a call from a Klipper macro, as `operator`: Klipper's connection can't tell who sent
/// the gcode. Moonraker's `shutdown_machine` and `reboot_machine` run as `admin` with
/// `[moonraker_compat] klipper_shutdown`, since Moonraker lets any macro call them.
pub async fn from_klipper(app: Arc<App>, method: String, params: Value) {
    let mut args = match params {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut access =
        Access { role: Some(Role::Operator), source: "Klipper".into(), client: "Klipper".into(), ..Access::default() };
    let name = if method == RUN_ACTION {
        match args.remove("action") {
            Some(Value::String(name)) => name,
            _ => {
                tracing::info!("Klipper called {RUN_ACTION} without an action name");
                return;
            }
        }
    } else {
        let Some((_, action, admin)) = MOONRAKER_REMOTE_METHODS.iter().find(|m| m.0 == method) else { return };
        if *admin && app.compat.klipper_shutdown {
            access.role = Some(Role::Admin);
            access.source = format!("Klipper's {method}, [moonraker_compat] klipper_shutdown");
        }
        // Moonraker calls its Python function with the arguments as they are, and `None` is
        // what an argument left out gets: `spool_id=None` clears the active spool, as the
        // macro in Moonraker's docs does. An action's method would refuse `null` instead.
        args.retain(|_, v| !v.is_null());
        (*action).to_owned()
    };
    // `run` logs the outcome. Klipper doesn't wait for a reply.
    let _ = run(&app, &access, &name, args).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(toml: &str) -> (Actions, Vec<String>) {
        let mut c = Config::parse("[spoolman]\nserver: http://spoolman.local\n").unwrap();
        c.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
        let (agents, _) = Agents::new(&c, None).unwrap();
        Actions::new(&c, &agents, Vec::new())
    }

    #[test]
    fn names_full_and_short() {
        assert_eq!(parse_name("save_state"), Some((None, "save_state")));
        assert_eq!(parse_name("action.save_state"), Some((None, "save_state")));
        assert_eq!(parse_name("power.printer_toggle"), Some((Some("power"), "printer_toggle")));
        assert_eq!(parse_name("agent.power.action.printer_toggle"), Some((Some("power"), "printer_toggle")));
        for bad in ["", "a.b.c", "agent.power", "action.", "agent.power.actions.x", "a b"] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn tables_and_built_ins() {
        let (a, w) = actions(
            r#"
[action.save_state]
gcode = "POWER_LOSS_SAVE"

[action.shutdown]
method = "machine.shutdown"
role = "admin"

[agent.power]
[agent.power.action.printer_toggle]
method = "post_device"
params = { device = "printer", action = "toggle" }
"#,
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(a.get("save_state").unwrap().step, Step::Gcode("POWER_LOSS_SAVE".into()));
        // A table replaces the built-in.
        assert_eq!(a.get("action.shutdown").unwrap().role, Role::Admin);
        assert_eq!(a.get("emergency_stop").unwrap().role, Role::Operator);
        let toggle = a.get("agent.power.action.printer_toggle").unwrap();
        assert_eq!(toggle, a.get("power.printer_toggle").unwrap());
        let (method, params) = toggle.method();
        assert_eq!((method.as_str(), params["action"].as_str()), ("agent.power.post_device", Some("toggle")));
        // The built-in Spoolman agent's.
        assert_eq!(a.get("spoolman.set_active_spool").unwrap().method().0, "agent.spoolman.post_spool_id");
        assert!(a.get("power.nothing").is_none() && a.get("nothing").is_none());
    }

    #[test]
    fn bad_tables_warn() {
        let (a, w) = actions(
            r#"
[action.both]
method = "machine.reboot"
gcode = "G28"

[action.neither]
role = "admin"

[action.typo]
mehtod = "machine.reboot"

[action.bad_role]
method = "machine.reboot"
role = "root"

[agent.power.action.long]
method = "agent.power.post_device"
"#,
        );
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w.iter().any(|w| w.contains("[agent.power.action.long]")), "{w:?}");
        assert!(a.get("both").is_none() && a.get("typo").is_none());
    }

    #[test]
    fn unknown_agent_has_no_actions() {
        // No [agent.power] table: an ad hoc agent has none.
        let (a, w) = actions("[agent.spoolman.action.x]\nmethod = \"status\"\n");
        assert!(w.is_empty());
        assert!(a.get("spoolman.x").is_some());
        let (a, _) = actions("");
        assert!(a.get("power.printer_toggle").is_none());
    }
}
