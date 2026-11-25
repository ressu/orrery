//! Agents: programs that connect over the websocket or the unix socket and identify with
//! `type: "agent"`, after Moonraker's `extensions` component. An agent can register Klippy
//! remote methods, send events that clients get as `notify_agent_event`, and answer calls that
//! clients make with `server.extensions.request`.
//!
//! One difference from Moonraker: a reply of `"result": null` from an agent is a success here.
//! Moonraker reads a missing `result` and a `null` one the same way, and turns both into an
//! agent error (`agent-null-result` in docs/regressions.md).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::api::{ApiError, ApiResult, ConnId, Request, RequestType, Transports, handler};
use crate::klippy::KlippyState;
use crate::server::App;

/// Events the server sends itself when an agent connects or disconnects.
const RESERVED_EVENTS: &[&str] = &["connected", "disconnected"];

type Reply = Result<Value, CallError>;

/// Why a call to an agent failed.
pub enum CallError {
    /// The agent replied with this JSON-RPC error.
    Agent(Value),
    /// The agent never replied, such as when it disconnected.
    Server(ApiError),
}

#[derive(Default)]
pub struct Extensions {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Connected agents by name, in the order they identified, as `server.extensions.list`
    /// reports them.
    agents: Vec<(String, ConnId)>,
    /// When each connected agent connected.
    connected_at: HashMap<String, std::time::Instant>,
    /// Klippy remote methods agents registered, and the agent each one goes to.
    methods: HashMap<String, ConnId>,
    /// Calls sent to agents and waiting for a reply, by connection and request id.
    pending: HashMap<(ConnId, u64), oneshot::Sender<Reply>>,
    next_id: u64,
}

impl Extensions {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("extensions lock")
    }

    /// The agent connection a Klippy remote method goes to.
    pub fn method_conn(&self, method: &str) -> Option<ConnId> {
        self.lock().methods.get(method).copied()
    }

    /// When the agent connected, if it's connected.
    pub fn connected_at(&self, agent: &str) -> Option<std::time::Instant> {
        self.lock().connected_at.get(agent).copied()
    }

    /// Every remote method agents registered, to register again when Klippy becomes ready.
    pub fn remote_methods(&self) -> Vec<String> {
        self.lock().methods.keys().cloned().collect()
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register(
        "/connection/register_remote_method",
        &[RequestType::Post],
        Transports::WebsocketOnly,
        handler(register_remote_method),
    );
    r.register("/connection/send_event", &[RequestType::Post], Transports::WebsocketOnly, handler(send_event));
    r.register("/server/extensions/list", &[RequestType::Get], Transports::All, handler(list));
    r.register("/server/extensions/request", &[RequestType::Post], Transports::All, handler(call_agent));
}

/// Registers a connection that identified as an agent, and tells the other clients.
pub async fn register_agent(app: &App, conn: ConnId, name: &str, identity: &Value) -> Result<(), ApiError> {
    crate::agents::check_claim(app, name, conn).await?;
    {
        let mut inner = app.extensions.lock();
        if inner.agents.iter().any(|(n, _)| n == name) {
            return Err(ApiError::bad_request(format!("Agent '{name}' already registered and connected")));
        }
        inner.agents.push((name.to_owned(), conn));
        inner.connected_at.insert(name.to_owned(), std::time::Instant::now());
    }
    tracing::info!("Agent {name} connected");
    if let Some(warning) = app.agents.version_warning(name, identity["version"].as_str()) {
        app.set_version_warning(name, Some(warning));
    }
    let event = json!({"agent": name, "event": "connected", "data": identity});
    app.notify_except("agent_event", vec![event], conn);
    Ok(())
}

/// Cleans up after a connection closes: its calls in flight fail, and an agent's remote methods
/// go away, and the other clients hear that it disconnected.
pub fn remove_connection(app: &App, conn: ConnId) {
    let (pending, name) = {
        let mut inner = app.extensions.lock();
        let keys: Vec<_> = inner.pending.keys().filter(|(c, _)| *c == conn).copied().collect();
        let pending: Vec<_> = keys.iter().filter_map(|k| inner.pending.remove(k)).collect();
        inner.methods.retain(|_, c| *c != conn);
        let name = inner.agents.iter().position(|&(_, c)| c == conn).map(|i| inner.agents.remove(i).0);
        if let Some(name) = &name {
            inner.connected_at.remove(name);
        }
        (pending, name)
    };
    for tx in pending {
        let _ = tx.send(Err(CallError::Server(ApiError::new(500, "Client Socket Disconnected"))));
    }
    if let Some(name) = name {
        crate::agents::remove_agent(app, &name);
        tracing::info!("Agent {name} disconnected");
        app.notify_except("agent_event", vec![json!({"agent": name, "event": "disconnected"})], conn);
    }
}

/// Passes a Klippy remote method call to the agent that registered it. Moonraker sends it as a
/// notification: the agent doesn't reply.
pub fn forward_remote_method(app: &App, conn: ConnId, method: &str, params: &Value) {
    let mut msg = json!({"jsonrpc": "2.0", "method": method});
    if is_truthy(params) {
        msg["params"] = params.clone();
    }
    app.send_to(conn, &msg);
}

/// Handles a client's reply to a call orrery sent it. Returns nothing: the reply goes to the
/// request that's waiting for it.
pub fn resolve_response(app: &App, conn: ConnId, obj: &Value) {
    let Some(id) = obj.get("id").and_then(Value::as_u64) else {
        tracing::debug!("RPC Response with null ID: {obj}");
        return;
    };
    let Some(tx) = app.extensions.lock().pending.remove(&(conn, id)) else {
        tracing::debug!("RPC Response with no matching request: {obj}");
        return;
    };
    let reply = match (obj.get("error"), obj.get("result")) {
        (None, Some(result)) => Ok(result.clone()),
        (error, _) => Err(CallError::Agent(error.cloned().unwrap_or_default())),
    };
    let _ = tx.send(reply);
}

/// Python's truthiness for JSON: Moonraker leaves `params` out when they're empty.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        _ => true,
    }
}

/// The identity of the agent behind a request, or Moonraker's error for a non-agent.
fn require_agent(app: &App, req: &Request, what: &str) -> Result<(ConnId, String), ApiError> {
    let conn = req.conn.ok_or_else(|| ApiError::bad_request("No connection detected"))?;
    let identity = app.client_identity(conn).unwrap_or_default();
    if identity["type"].as_str() != Some("agent") {
        return Err(ApiError::bad_request(format!("Only connections of the 'agent' type can {what}")));
    }
    Ok((conn, identity["name"].as_str().unwrap_or_default().to_owned()))
}

async fn register_remote_method(app: Arc<App>, req: Request) -> ApiResult {
    let conn = req.conn.ok_or_else(|| ApiError::bad_request("No connection detected"))?;
    let method = req.args.str("method_name")?;
    require_agent(&app, &req, "register methods")?;
    {
        let mut inner = app.extensions.lock();
        if crate::actions::is_remote_method(&app, &method) || inner.methods.contains_key(&method) {
            return Err(ApiError::bad_request(format!("Remote method ({method}) already registered")));
        }
        inner.methods.insert(method.clone(), conn);
    }
    // When Klippy isn't ready yet, the method is registered with the others once it is.
    if app.klippy.state().0 == KlippyState::Ready && !app.klippy.is_initializing() {
        let app = app.clone();
        tokio::spawn(async move { crate::klippy::register_remote_method(&app, &method).await });
    }
    Ok(json!("ok"))
}

async fn send_event(app: Arc<App>, req: Request) -> ApiResult {
    let (conn, name) = require_agent(&app, &req, "send events")?;
    let event = req.args.str("event")?;
    if RESERVED_EVENTS.contains(&event.as_str()) {
        return Err(ApiError::bad_request(format!("Event '{event}' is reserved")));
    }
    let data = req.args.get("data").filter(|d| !d.is_null()).cloned();
    // orrery.toml can give an event a public name, such as `notify_active_spool_set`.
    if let Some(public) = app.agents.notification(&name, &event) {
        app.notify_except(public, data.into_iter().collect(), conn);
        return Ok(json!("ok"));
    }
    let mut evt = json!({"agent": name, "event": event});
    if let Some(data) = data {
        evt["data"] = data;
    }
    app.notify_except("agent_event", vec![evt], conn);
    Ok(json!("ok"))
}

async fn list(app: Arc<App>, _req: Request) -> ApiResult {
    let conns: Vec<ConnId> = app.extensions.lock().agents.iter().map(|&(_, c)| c).collect();
    let agents: Vec<Value> = conns.into_iter().filter_map(|c| app.client_identity(c)).collect();
    Ok(json!({"agents": agents}))
}

async fn call_agent(app: Arc<App>, req: Request) -> ApiResult {
    let agent = req.args.str("agent")?;
    let method = req.args.str("method")?;
    let arguments = req.args.get("arguments").cloned().unwrap_or_default();
    if !matches!(arguments, Value::Null | Value::Array(_) | Value::Object(_)) {
        return Err(ApiError::bad_request("The 'arguments' field must contain an object or a list"));
    }
    call(&app, &agent, &method, arguments).await
}

pub fn is_connected(app: &App, agent: &str) -> bool {
    app.extensions.lock().agents.iter().any(|(n, _)| n == agent)
}

/// Calls a method on an agent and waits for its reply, as `server.extensions.request` does: an
/// error from the agent becomes Moonraker's 424, with the agent's error as `data`.
pub async fn call(app: &App, agent: &str, method: &str, arguments: Value) -> ApiResult {
    send_call(app, agent, method, arguments).await.map_err(|e| match e {
        CallError::Agent(error) => ApiError::new(424, format!("Agent {agent} RPC error")).with_data(error),
        CallError::Server(e) => e,
    })
}

/// Calls a method on an agent and waits for its reply.
pub async fn send_call(app: &App, agent: &str, method: &str, arguments: Value) -> Result<Value, CallError> {
    let (tx, rx) = oneshot::channel();
    // The lookup and the pending entry share one lock, so an agent that disconnects in between
    // can't leave the call waiting forever.
    let (conn, id) = {
        let mut inner = app.extensions.lock();
        let conn = inner.agents.iter().find(|(n, _)| *n == agent).map(|&(_, c)| c);
        let conn =
            conn.ok_or_else(|| CallError::Server(ApiError::bad_request(format!("Agent {agent} not connected"))))?;
        inner.next_id += 1;
        let id = inner.next_id;
        inner.pending.insert((conn, id), tx);
        (conn, id)
    };
    let mut msg = json!({"jsonrpc": "2.0", "method": method, "id": id});
    if is_truthy(&arguments) {
        msg["params"] = arguments;
    }
    app.send_to(conn, &msg);
    // Moonraker waits without a time limit; a call ends when the agent replies or disconnects.
    match rx.await {
        Ok(reply) => reply,
        Err(_) => Err(CallError::Server(ApiError::new(500, "Client Socket Disconnected"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness() {
        assert!(!is_truthy(&json!(null)));
        assert!(!is_truthy(&json!({})));
        assert!(!is_truthy(&json!([])));
        assert!(is_truthy(&json!([0])));
        assert!(is_truthy(&json!({"a": 1})));
    }
}
