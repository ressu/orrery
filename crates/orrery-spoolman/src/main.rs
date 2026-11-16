//! orrery-spoolman: Moonraker's Spoolman integration as an orrery agent.
//!
//! It does what Moonraker's `spoolman` component does: it follows the extruder's position,
//! reports the filament used to Spoolman every `sync_rate` seconds, keeps the active spool, and
//! answers `SET_ACTIVE_SPOOL` macros. Its endpoints are `/agent/spoolman/spool_id`, `proxy`, and
//! `status`. With this table in `orrery.toml`, Fluidd and Mainsail reach them under Moonraker's
//! names:
//!
//! ```toml
//! [agent.spoolman]
//! component = "spoolman"
//! history_fields = ["spool_ids"]
//!
//! [agent.spoolman.maps]
//! "server.spoolman" = "agent.spoolman"
//! "notify_active_spool_set" = "agent.spoolman.active_spool_set"
//! "notify_spoolman_status_changed" = "agent.spoolman.spoolman_status_changed"
//! ```
//!
//! Its settings are the `[spoolman]` section, in `moonraker.conf` or `orrery.toml`: `server`
//! (required) and `sync_rate` (seconds, default 5). It keeps the active spool in orrery's
//! database, in the `spoolman` namespace. On its first run it takes the spool Moonraker had, from
//! `moonraker` / `spoolman.spool_id`, which `orrery-import` brings over.

mod server;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orrery_agent::{Agent, AgentBuilder, HistoryField, MethodError};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use server::Server;

const NAME: &str = "spoolman";
const HISTORY_FIELD: &str = "spool_ids";
/// Moonraker's history keeps at most this many spools per job.
const MAX_JOB_SPOOLS: usize = 100;

struct Spoolman {
    /// Set from the config the first time the agent connects.
    server: OnceLock<Server>,
    sync_rate: OnceLock<Duration>,
    /// The connection to orrery, when there is one.
    agent: Mutex<Option<Agent>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    spool_id: Option<i64>,
    /// Filament used per spool, in mm, not yet reported to Spoolman.
    pending: Vec<(i64, f64)>,
    highest_epos: f64,
    last_epos: f64,
    extruder: String,
    /// Whether the websocket to Spoolman is open.
    connected: bool,
    error_logged: bool,
    /// The spools used in the print job being recorded, for its `spool_ids` field.
    job_spools: Option<Vec<Value>>,
}

impl State {
    fn add_extrusion(&mut self, spool: i64, used: f64) {
        match self.pending.iter_mut().find(|(id, _)| *id == spool) {
            Some((_, total)) => *total += used,
            None => self.pending.push((spool, used)),
        }
    }

    fn drop_pending(&mut self, spool: i64) {
        self.pending.retain(|(id, _)| *id != spool);
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let spoolman = Arc::new(Spoolman {
        server: OnceLock::new(),
        sync_rate: OnceLock::new(),
        agent: Mutex::new(None),
        state: Mutex::new(State { extruder: "extruder".into(), ..State::default() }),
    });
    let mut builder = AgentBuilder::new(NAME).version(env!("CARGO_PKG_VERSION"));
    if let Some(socket) = std::env::args().nth(1) {
        builder = builder.socket(socket);
    }
    let s = spoolman.clone();
    builder = builder.method("get_spool_id", move |_| {
        let s = s.clone();
        async move { Ok(json!({"spool_id": s.state().spool_id})) }
    });
    let s = spoolman.clone();
    builder = builder.method("post_spool_id", move |params| {
        let s = s.clone();
        async move {
            let spool_id = match params.get("spool_id") {
                None => None,
                Some(v) => Some(to_int("spool_id", v)?),
            };
            s.set_active_spool(spool_id).await;
            Ok(json!({"spool_id": s.state().spool_id}))
        }
    });
    let s = spoolman.clone();
    builder = builder.method("status", move |_| {
        let s = s.clone();
        async move {
            let st = s.state();
            let pending: Vec<Value> =
                st.pending.iter().map(|(id, used)| json!({"spool_id": id, "filament_used": used})).collect();
            Ok(json!({"spoolman_connected": st.connected, "pending_reports": pending, "spool_id": st.spool_id}))
        }
    });
    let s = spoolman.clone();
    builder = builder.method("proxy", move |params| {
        let s = s.clone();
        async move { s.proxy(params).await }
    });
    // Klippy's SET_ACTIVE_SPOOL and CLEAR_ACTIVE_SPOOL macros.
    let s = spoolman.clone();
    builder = builder.method("spoolman_set_active_spool", move |params| {
        let s = s.clone();
        async move {
            let spool_id = match params.get("spool_id") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64().ok_or_else(|| MethodError::new(400, format!("Invalid spool id: {v}")))?),
            };
            s.set_active_spool(spool_id).await;
            Ok(Value::Null)
        }
    });
    let s = spoolman.clone();
    orrery_agent::run(builder, move |agent| {
        let s = s.clone();
        async move { s.setup(agent).await }
    })
    .await
}

impl Spoolman {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("state lock")
    }

    fn agent(&self) -> Option<Agent> {
        self.agent.lock().expect("agent lock").clone()
    }

    fn server(&self) -> &Server {
        self.server.get().expect("set up on the first connection")
    }

    /// Runs each time the agent connects to orrery.
    async fn setup(self: &Arc<Self>, agent: Agent) -> Result<(), orrery_agent::Error> {
        *self.agent.lock().expect("agent lock") = Some(agent.clone());
        if self.server.get().is_none() {
            self.start(&agent).await?;
        }
        agent.register_endpoint("spool_id", &["GET", "POST"]).await?;
        agent.register_endpoint("proxy", &["POST"]).await?;
        agent.register_endpoint("status", &["GET"]).await?;
        agent.register_remote_method("spoolman_set_active_spool").await?;

        let s = self.clone();
        let mut notifications = agent.notifications();
        let a = agent.clone();
        tokio::spawn(async move {
            loop {
                let n = tokio::select! {
                    n = notifications.recv() => n,
                    () = a.closed() => break,
                };
                let n = match n {
                    Ok(n) => n,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                match n.method.as_str() {
                    "notify_status_update" => s.status_update(&n.params[0]),
                    "notify_klippy_ready" => {
                        let s = s.clone();
                        tokio::spawn(async move { s.subscribe().await });
                    }
                    "notify_history_changed" => s.history_changed(&n.params[0]).await,
                    _ => {}
                }
            }
        });

        let info = agent.call("server.info", Value::Null).await?;
        if info["klippy_state"] == "ready" {
            self.subscribe().await;
        }
        // A job may already be printing, such as after orrery restarted.
        let last = agent.call("server.history.list", json!({"limit": 1})).await?;
        let job = &last["jobs"][0];
        if job["status"] == "in_progress" {
            let field = job["auxiliary_data"].as_array().into_iter().flatten().find(|f| f["name"] == HISTORY_FIELD);
            let spools = match field.and_then(|f| f["value"].as_array()) {
                Some(spools) => spools.clone(),
                None => self.state().spool_id.map(|id| vec![json!(id)]).unwrap_or_default(),
            };
            self.state().job_spools = Some(spools);
        }
        Ok(())
    }

    /// Reads the config and the active spool, and starts talking to Spoolman.
    async fn start(self: &Arc<Self>, agent: &Agent) -> Result<(), orrery_agent::Error> {
        let setup_error = |message: String| orrery_agent::Error::Rpc {
            method: "orrery.agent.config".into(),
            code: 400,
            message,
            data: None,
        };
        let config = agent.config(None).await?.ok_or_else(|| {
            setup_error("orrery has no [spoolman] section; add one with the Spoolman server's URL".into())
        })?;
        let url = config.get("server").and_then(Value::as_str).ok_or_else(|| {
            setup_error("[spoolman] has no server option; set it to the Spoolman server's URL".into())
        })?;
        let sync_rate = match config.get("sync_rate") {
            None => 5,
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
            Some(_) => 0,
        };
        if sync_rate < 1 {
            return Err(setup_error(format!(
                "[spoolman] sync_rate: {} must be at least 1 second",
                config["sync_rate"]
            )));
        }
        let server = Server::new(url).map_err(setup_error)?;

        // The active spool: orrery's own, or the one Moonraker had.
        let mut spool_id = None;
        for (namespace, key) in [(NAME, "spool_id"), ("moonraker", "spoolman.spool_id")] {
            if let Ok(item) = agent.call("server.database.get_item", json!({"namespace": namespace, "key": key})).await
            {
                spool_id = item["value"].as_i64();
                if namespace == "moonraker" {
                    tracing::info!("Took the active spool from Moonraker's database: {}", show(spool_id));
                }
                break;
            }
        }
        self.state().spool_id = spool_id;
        tracing::info!("Spoolman server {}, active spool {}, sync every {sync_rate} s", server.api, show(spool_id));
        let _ = self.server.set(server);
        let _ = self.sync_rate.set(Duration::from_secs(sync_rate));

        let s = self.clone();
        tokio::spawn(async move { s.websocket_loop().await });
        let s = self.clone();
        tokio::spawn(async move { s.report_loop().await });
        Ok(())
    }

    /// Follows the extruder, as Moonraker does when Klippy becomes ready.
    async fn subscribe(&self) {
        let Some(agent) = self.agent() else { return };
        let objects = json!({"objects": {"toolhead": ["position", "extruder"]}});
        let toolhead = match agent.call("printer.objects.subscribe", objects).await {
            Ok(reply) => reply["status"]["toolhead"].clone(),
            Err(e) => {
                tracing::warn!("Can't follow the extruder: {e}");
                return;
            }
        };
        let mut st = self.state();
        st.extruder = toolhead["extruder"].as_str().unwrap_or("extruder").to_owned();
        match toolhead["position"][3].as_f64() {
            Some(epos) => st.highest_epos = epos,
            None => tracing::error!("Can't follow the extruder: Klippy didn't report its position"),
        }
    }

    fn status_update(&self, status: &Value) {
        let Some(toolhead) = status.get("toolhead") else { return };
        let mut st = self.state();
        let epos = toolhead.get("position").and_then(|p| p[3].as_f64()).unwrap_or(st.highest_epos);
        st.last_epos = epos;
        let extruder = toolhead["extruder"].as_str().unwrap_or(&st.extruder).to_owned();
        if extruder != st.extruder {
            st.highest_epos = epos;
            st.extruder = extruder;
        } else if epos > st.highest_epos {
            if let Some(spool) = st.spool_id {
                let used = epos - st.highest_epos;
                st.add_extrusion(spool, used);
            }
            st.highest_epos = epos;
        }
    }

    /// A job starts with the active spool in its `spool_ids`, as Moonraker's does.
    async fn history_changed(&self, change: &Value) {
        let field = {
            let mut st = self.state();
            match change["action"].as_str() {
                Some("added") => {
                    let spools: Vec<Value> = st.spool_id.map(|id| vec![json!(id)]).unwrap_or_default();
                    st.job_spools = Some(spools.clone());
                    spools
                }
                Some("finished") => {
                    st.job_spools = None;
                    return;
                }
                _ => return,
            }
        };
        self.send_history_field(field).await;
    }

    async fn send_history_field(&self, spools: Vec<Value>) {
        let Some(agent) = self.agent() else { return };
        let field = HistoryField {
            name: HISTORY_FIELD.into(),
            value: Value::Array(spools),
            description: Some("Spool IDs used".into()),
            units: None,
        };
        if let Err(e) = agent.history_field(field).await {
            tracing::warn!("Can't record the job's spools: {e}");
        }
    }

    async fn send_event(&self, event: &str, data: Value) {
        if let Some(agent) = self.agent() {
            let _ = agent.send_event(event, Some(data)).await;
        }
    }

    async fn set_active_spool(&self, spool_id: Option<i64>) {
        let job_spools = {
            let mut st = self.state();
            if st.spool_id == spool_id {
                tracing::info!("The active spool is already {}", show(spool_id));
                return;
            }
            st.spool_id = spool_id;
            st.highest_epos = st.last_epos;
            st.job_spools.as_mut().and_then(|spools| {
                let id = json!(spool_id);
                if spools.contains(&id) {
                    return None;
                }
                spools.push(id);
                if spools.len() > MAX_JOB_SPOOLS {
                    spools.remove(0);
                }
                Some(spools.clone())
            })
        };
        tracing::info!("Set the active spool to {}", show(spool_id));
        if let Some(agent) = self.agent() {
            let item = json!({"namespace": NAME, "key": "spool_id", "value": spool_id});
            if let Err(e) = agent.call("server.database.post_item", item).await {
                tracing::warn!("Can't save the active spool: {e}");
            }
        }
        self.send_event("active_spool_set", json!({"spool_id": spool_id})).await;
        if let Some(spools) = job_spools {
            self.send_history_field(spools).await;
        }
    }

    async fn send_status(&self) {
        let connected = self.state().connected;
        self.send_event("spoolman_status_changed", json!({"spoolman_connected": connected})).await;
    }

    /// Keeps the websocket to Spoolman open. Spoolman reports deleted spools there, and orrery
    /// reports filament use only while it's open, as Moonraker does.
    async fn websocket_loop(self: Arc<Self>) {
        let mut last_error = None;
        loop {
            match self.server().connect_ws().await {
                Err(e) => {
                    // One line per new error, not one every two seconds.
                    if last_error.as_ref() != Some(&e) {
                        tracing::warn!("Can't connect to Spoolman at {}: {e}", self.server().ws);
                        last_error = Some(e);
                    }
                }
                Ok(socket) => {
                    last_error = None;
                    let spool = {
                        let mut st = self.state();
                        st.connected = true;
                        st.error_logged = false;
                        st.spool_id
                    };
                    tracing::info!("Connected to Spoolman at {}", self.server().ws);
                    if let Some(spool) = spool {
                        let s = self.clone();
                        tokio::spawn(async move { s.check_spool(spool).await });
                    }
                    self.send_status().await;
                    let reason = self.read_websocket(socket).await;
                    self.state().connected = false;
                    tracing::info!("Disconnected from Spoolman: {reason}");
                    self.send_status().await;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Reads Spoolman's messages until the websocket closes, and says why it closed.
    async fn read_websocket(&self, socket: server::Socket) -> String {
        let (mut tx, mut rx) = socket.split();
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        ping.tick().await;
        loop {
            let msg = tokio::select! {
                msg = rx.next() => msg,
                _ = ping.tick() => {
                    if tx.send(Message::Ping(Default::default())).await.is_err() {
                        return "the connection failed".into();
                    }
                    continue;
                }
            };
            match msg {
                Some(Ok(Message::Text(text))) => self.spoolman_event(&text).await,
                Some(Ok(Message::Close(frame))) => {
                    return frame.map_or("Spoolman closed the connection".into(), |f| {
                        format!("Spoolman closed the connection (code {}, {:?})", u16::from(f.code), f.reason.as_str())
                    });
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return e.to_string(),
                None => return "the connection closed".into(),
            }
        }
    }

    async fn spoolman_event(&self, text: &str) {
        let Ok(event) = serde_json::from_str::<Value>(text) else { return };
        if event["resource"] != "spool" || event["type"] != "deleted" {
            return;
        }
        let deleted = event["payload"]["id"].as_i64();
        let active = self.state().spool_id;
        if active.is_some() && deleted == active {
            tracing::info!("Spoolman deleted the active spool {}", show(active));
            self.state().drop_pending(deleted.unwrap_or_default());
            self.set_active_spool(None).await;
        }
    }

    /// Clears the active spool if Spoolman no longer has it.
    async fn check_spool(&self, spool: i64) {
        let reply =
            self.server().request("GET", &format!("/v1/spool/{spool}"), None, Some(Duration::from_secs(2))).await;
        match reply {
            Ok(r) if r.status == 404 => {
                tracing::info!("Spoolman has no spool {spool}, so there's no active spool now");
                self.state().drop_pending(spool);
                self.set_active_spool(None).await;
            }
            Ok(r) if r.has_error() => tracing::info!("Can't check spool {spool}: {}", r.error_text()),
            Ok(_) => tracing::info!("Spoolman has the active spool {spool}"),
            Err(e) => tracing::info!("Can't check spool {spool}: {e}"),
        }
    }

    /// Reports the filament used every `sync_rate` seconds, while Spoolman is connected.
    async fn report_loop(self: Arc<Self>) {
        let rate = *self.sync_rate.get().expect("set before the loop starts");
        loop {
            tokio::time::sleep(rate).await;
            let pending = {
                let mut st = self.state();
                if !st.connected {
                    continue;
                }
                std::mem::take(&mut st.pending)
            };
            for (spool, used) in pending {
                if !self.state().connected {
                    self.state().add_extrusion(spool, used);
                    continue;
                }
                tracing::debug!("Reporting {used:.3} mm used from spool {spool}");
                let body = json!({"use_length": used});
                let reply = self.server().request("PUT", &format!("/v1/spool/{spool}/use"), Some(&body), None).await;
                match reply {
                    Ok(r) if r.status == 404 => {
                        self.state().drop_pending(spool);
                        if self.state().spool_id == Some(spool) {
                            tracing::info!("Spoolman has no spool {spool}, so there's no active spool now");
                            self.set_active_spool(None).await;
                        }
                    }
                    Ok(r) if !r.has_error() => {}
                    failed => {
                        let mut st = self.state();
                        if !st.error_logged {
                            let error = match failed {
                                Ok(r) => r.error_text(),
                                Err(e) => e,
                            };
                            tracing::info!("Can't report filament used from spool {spool}: {error}");
                            st.error_logged = true;
                        }
                        // Reported again on the next round.
                        st.add_extrusion(spool, used);
                        continue;
                    }
                }
                self.state().error_logged = false;
            }
        }
    }

    /// `server.spoolman.proxy`: passes a request on to Spoolman's API.
    async fn proxy(&self, params: Value) -> Result<Value, MethodError> {
        let method = to_str(&params, "request_method")?;
        let path = to_str(&params, "path")?;
        let query = params.get("query").map(|_| to_str(&params, "query")).transpose()?;
        let body = params.get("body").filter(|b| !b.is_null());
        let v2 = match params.get("use_v2_response") {
            None => false,
            Some(v) => to_bool("use_v2_response", v)?,
        };
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
            return Err(MethodError::new(400, format!("Invalid HTTP method: {method}")));
        }
        if body.is_some() && method == "GET" {
            return Err(MethodError::new(400, "GET requests cannot have a body"));
        }
        if !path.starts_with("/v1/") {
            return Err(MethodError::new(400, "Invalid path, must start with the API version, e.g. /v1"));
        }
        let target = match &query {
            Some(q) => format!("{path}?{q}"),
            None => path,
        };
        let v2_error = |status: u16, message: String| json!({"response": null, "error": {"status_code": status, "message": message}});
        if !self.state().connected {
            if !v2 {
                return Err(MethodError::new(503, "Spoolman server not available"));
            }
            return Ok(v2_error(503, "Spoolman server not available".into()));
        }
        tracing::debug!("Passing {method} {target} on to Spoolman");
        let reply = match self.server().request(&method, &target, body, None).await {
            Ok(r) => r,
            Err(e) => {
                if !v2 {
                    return Err(MethodError::new(500, format!("HTTP Request Error: {}{target}", self.server().api)));
                }
                return Ok(v2_error(500, e));
            }
        };
        if reply.has_error() {
            if !v2 {
                return Err(MethodError::new(i64::from(reply.status), reply.reason()));
            }
            let message = reply.message().unwrap_or_else(|| format!("HTTP {}: {}", reply.status, reply.reason()));
            return Ok(v2_error(reply.status, message));
        }
        let json = reply.json().ok_or_else(|| MethodError::new(500, "Spoolman's reply isn't JSON"))?;
        if !v2 {
            return Ok(json);
        }
        Ok(json!({"response": json, "response_headers": reply.headers, "error": null}))
    }
}

fn show(spool: Option<i64>) -> String {
    spool.map_or("none".into(), |id| id.to_string())
}

/// A value as Python prints it, for Moonraker's conversion errors.
fn python(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn conversion_error(key: &str, kind: &str, v: &Value) -> MethodError {
    MethodError::new(
        400,
        format!("Unable to convert argument [{key}] to <class '{kind}'>: value received: {}", python(v)),
    )
}

/// Python's `int()`, as Moonraker's `get_int` uses it.
fn to_int(key: &str, v: &Value) -> Result<i64, MethodError> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => {
            n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)).ok_or_else(|| conversion_error(key, "int", v))
        }
        Value::String(s) => s.trim().parse().map_err(|_| conversion_error(key, "int", v)),
        _ => Err(conversion_error(key, "int", v)),
    }
}

/// Moonraker's `get_str`: any value converts, and a missing one is an error.
fn to_str(params: &Value, key: &str) -> Result<String, MethodError> {
    params.get(key).map(python).ok_or_else(|| MethodError::new(400, format!("No data for argument: {key}")))
}

fn to_bool(key: &str, v: &Value) -> Result<bool, MethodError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(conversion_error(key, "bool", v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_like_moonraker() {
        assert_eq!(to_int("spool_id", &json!("3")).unwrap(), 3);
        assert_eq!(to_int("spool_id", &json!(3.7)).unwrap(), 3);
        assert_eq!(
            to_int("spool_id", &json!(null)).unwrap_err().message,
            "Unable to convert argument [spool_id] to <class 'int'>: value received: None"
        );
        assert_eq!(to_str(&json!({"path": 5}), "path").unwrap(), "5");
        assert_eq!(to_str(&json!({}), "path").unwrap_err().message, "No data for argument: path");
        assert!(to_bool("x", &json!("TRUE")).unwrap());
    }

    #[test]
    fn counts_extrusion_per_spool() {
        let mut st = State::default();
        st.add_extrusion(1, 2.0);
        st.add_extrusion(2, 1.0);
        st.add_extrusion(1, 0.5);
        assert_eq!(st.pending, vec![(1, 2.5), (2, 1.0)]);
        st.drop_pending(1);
        assert_eq!(st.pending, vec![(2, 1.0)]);
    }
}
