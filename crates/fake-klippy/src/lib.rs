//! A scriptable stand-in for Klipper's unix socket API (`klippy.py -a <socket>`).
//!
//! It speaks the same protocol as Klipper's `webhooks.py`, serves printer objects recorded from
//! a real Klipper (see [`Capture`]), and lets tests drive it through [`FakeKlippy`]: change object
//! fields, emit gcode output, switch printer state, call remote methods or drop the connection.
//!
//! Behaviour copied from Klipper:
//! - `objects/query` and `objects/subscribe` return `{"eventtime", "status"}`; requested fields
//!   that don't exist come back as `null`, and unknown objects as `{}`.
//! - Subscribers get only the fields that changed, wrapped in the `response_template` they sent.
//! - Responses with no data are `{}`. Errors are `{"error": "WebRequestError", "message": ...}`.
//! - RESTART and FIRMWARE_RESTART close every client connection and go through `startup`.

pub mod script;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orrery_proto::klippy;
use serde_json::{Map, Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use script::{Capture, GcodeRule, KlippyState, Script};

/// Klipper's `eventtime` is its reactor clock; the fake starts it here so values look plausible.
const EVENTTIME_BASE: f64 = 1000.0;

type ConnId = u64;

/// A `response_template` a client sent, to wrap messages pushed to it.
type Template = Map<String, Value>;

struct Conn {
    tx: mpsc::UnboundedSender<Value>,
    /// `objects/subscribe` state: requested objects (fields, or `None` for all) and template.
    subscription: Option<(Map<String, Value>, Map<String, Value>)>,
    gcode_template: Option<Map<String, Value>>,
}

struct State {
    script: Script,
    objects: Map<String, Value>,
    state: KlippyState,
    state_message: String,
    conns: HashMap<ConnId, Conn>,
    next_conn: ConnId,
    remote_methods: HashMap<String, Vec<(ConnId, Template)>>,
    requests: Vec<Value>,
    /// Bumped on every restart, so a pending "back to ready" timer from an older restart is ignored.
    generation: u64,
}

/// A running fake Klippy. Dropping it stops the server and removes the socket file.
pub struct FakeKlippy {
    shared: Arc<Mutex<State>>,
    started: Instant,
    socket: PathBuf,
    accept_task: JoinHandle<()>,
}

impl FakeKlippy {
    /// Starts listening on `socket`, replacing any stale socket file.
    pub async fn start(socket: impl AsRef<Path>, script: Script) -> std::io::Result<Self> {
        let socket = socket.as_ref().to_path_buf();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;

        let mut objects = script.capture.objects.clone();
        let target = script.state;
        let initial = if script.startup_ms > 0 { KlippyState::Startup } else { target };
        let message = state_message(initial, script.state_message.as_deref(), target);
        objects.insert("webhooks".into(), json!({"state": initial.as_str(), "state_message": message}));
        let startup_ms = script.startup_ms;

        let shared = Arc::new(Mutex::new(State {
            script,
            objects,
            state: initial,
            state_message: message,
            conns: HashMap::new(),
            next_conn: 0,
            remote_methods: HashMap::new(),
            requests: Vec::new(),
            generation: 0,
        }));
        let started = Instant::now();
        let accept_task = tokio::spawn(accept_loop(listener, shared.clone(), started));
        let fake = Self { shared, started, socket, accept_task };
        if startup_ms > 0 {
            fake.schedule_ready(Duration::from_millis(startup_ms), 0, target);
        }
        Ok(fake)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Changes printer object fields (`{object: {field: value}}`) and notifies subscribers of
    /// the fields that actually changed.
    pub fn update_objects(&self, patch: &Map<String, Value>) {
        let mut st = self.shared.lock().unwrap();
        apply_patch(&mut st, patch, eventtime(self.started));
    }

    /// Sends a line of gcode output to `gcode/subscribe_output` subscribers.
    pub fn gcode_output(&self, line: &str) {
        let st = self.shared.lock().unwrap();
        send_gcode_output(&st, line);
    }

    /// Switches printer state, updating the `webhooks` object like Klipper does.
    pub fn set_state(&self, state: KlippyState, message: Option<&str>) {
        let mut st = self.shared.lock().unwrap();
        set_state(&mut st, state, message.map(str::to_owned), eventtime(self.started));
    }

    /// Simulates a RESTART: drops every client, then reports `startup` until `restart_ms` passes.
    pub fn restart(&self) {
        let (generation, delay, target) = {
            let mut st = self.shared.lock().unwrap();
            begin_restart(&mut st, eventtime(self.started))
        };
        self.schedule_ready(delay, generation, target);
    }

    /// Closes every client connection without changing printer state, like a crashed Klippy
    /// that comes straight back.
    pub fn disconnect_clients(&self) {
        let mut st = self.shared.lock().unwrap();
        st.conns.clear();
        st.remote_methods.clear();
    }

    /// Calls a method a client registered with `register_remote_method`, like a gcode macro using
    /// `action_call_remote_method` would.
    pub fn call_remote_method(&self, method: &str, params: Value) -> Result<(), String> {
        let st = self.shared.lock().unwrap();
        call_remote_method(&st, method, params)
    }

    /// Every request received so far, as `{"method", "params"}`, oldest first.
    pub fn requests(&self) -> Vec<Value> {
        self.shared.lock().unwrap().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.shared.lock().unwrap().requests.clear();
    }

    /// Whether some client has an `objects/subscribe` that includes `object`.
    pub fn is_subscribed(&self, object: &str) -> bool {
        let st = self.shared.lock().unwrap();
        st.conns.values().any(|c| c.subscription.as_ref().is_some_and(|(objs, _)| objs.contains_key(object)))
    }

    pub fn client_count(&self) -> usize {
        self.shared.lock().unwrap().conns.len()
    }

    pub fn state(&self) -> KlippyState {
        self.shared.lock().unwrap().state
    }

    fn schedule_ready(&self, delay: Duration, generation: u64, target: KlippyState) {
        spawn_leave_startup(self.shared.clone(), self.started, delay, generation, target);
    }
}

impl Drop for FakeKlippy {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.shared.lock().unwrap().conns.clear();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn eventtime(started: Instant) -> f64 {
    EVENTTIME_BASE + started.elapsed().as_secs_f64()
}

fn state_message(state: KlippyState, custom: Option<&str>, target: KlippyState) -> String {
    match custom {
        Some(m) if state == target && state != KlippyState::Ready => m.to_owned(),
        _ => state.default_message().to_owned(),
    }
}

async fn accept_loop(listener: UnixListener, shared: Arc<Mutex<State>>, started: Instant) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (tx, rx) = mpsc::unbounded_channel();
        let id = {
            let mut st = shared.lock().unwrap();
            let id = st.next_conn;
            st.next_conn += 1;
            st.conns.insert(id, Conn { tx, subscription: None, gcode_template: None });
            id
        };
        tokio::spawn(serve_conn(stream, rx, id, shared.clone(), started));
    }
}

async fn serve_conn(
    stream: UnixStream,
    mut rx: mpsc::UnboundedReceiver<Value>,
    id: ConnId,
    shared: Arc<Mutex<State>>,
    started: Instant,
) {
    let (read, mut write) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    loop {
        tokio::select! {
            out = rx.recv() => {
                // `None`: the connection was dropped from the table (restart or disconnect).
                let Some(msg) = out else { break };
                if klippy::write_message(&mut write, &msg).await.is_err() {
                    break;
                }
            }
            msg = klippy::read_message(&mut reader) => match msg {
                Ok(Some(req)) => handle_request(&shared, id, req, started),
                Ok(None) => break,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    tracing::warn!("fake-klippy: undecodable request: {e}");
                }
                Err(_) => break,
            },
        }
    }
    let mut st = shared.lock().unwrap();
    st.conns.remove(&id);
    for conns in st.remote_methods.values_mut() {
        conns.retain(|(c, _)| *c != id);
    }
}

fn handle_request(shared: &Arc<Mutex<State>>, conn: ConnId, req: Value, started: Instant) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or_default().to_owned();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let now = eventtime(started);

    let mut st = shared.lock().unwrap();
    st.requests.push(json!({"method": method, "params": params}));
    let outcome = dispatch(&mut st, conn, &method, &params, now);
    let restart = matches!(outcome, Outcome::Restart);
    if let Some(id) = id {
        let reply = match &outcome {
            Outcome::Error(message) => json!({"id": id, "error": {"error": "WebRequestError", "message": message}}),
            Outcome::Ok(result) => json!({"id": id, "result": result}),
            Outcome::Restart => json!({"id": id, "result": {}}),
        };
        if let Some(c) = st.conns.get(&conn) {
            let _ = c.tx.send(reply);
        }
    }
    if restart {
        let (generation, delay, target) = begin_restart(&mut st, now);
        drop(st);
        spawn_leave_startup(shared.clone(), started, delay, generation, target);
    }
}

/// After `delay`, moves from `startup` to `target`, unless another restart happened meanwhile.
fn spawn_leave_startup(
    shared: Arc<Mutex<State>>,
    started: Instant,
    delay: Duration,
    generation: u64,
    target: KlippyState,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let mut st = shared.lock().unwrap();
        if st.generation == generation && st.state == KlippyState::Startup {
            let message = st.script.state_message.clone().filter(|_| target != KlippyState::Ready);
            set_state(&mut st, target, message, eventtime(started));
        }
    });
}

enum Outcome {
    Ok(Value),
    Error(String),
    Restart,
}

fn dispatch(st: &mut State, conn: ConnId, method: &str, params: &Value, now: f64) -> Outcome {
    let endpoint_known = method == "list_endpoints"
        || st.script.capture.endpoints.iter().any(|e| e == method)
        || matches!(method, "info" | "emergency_stop" | "register_remote_method");
    if !endpoint_known {
        return Outcome::Error(format!("webhooks: No registered callback for path '{method}'"));
    }
    match method {
        "info" => Outcome::Ok(info(st)),
        "list_endpoints" => {
            let mut eps: Vec<String> = ["list_endpoints", "info", "emergency_stop", "register_remote_method"]
                .into_iter()
                .map(String::from)
                .collect();
            eps.extend(st.script.capture.endpoints.iter().filter(|e| !eps.contains(e)).cloned().collect::<Vec<_>>());
            Outcome::Ok(json!({"endpoints": eps}))
        }
        "emergency_stop" => {
            set_state(st, KlippyState::Shutdown, None, now);
            Outcome::Ok(json!({}))
        }
        "register_remote_method" => {
            let (Some(method), Some(Value::Object(template))) =
                (params.get("remote_method").and_then(Value::as_str), params.get("response_template"))
            else {
                return Outcome::Error("Invalid argument".into());
            };
            let entry = st.remote_methods.entry(method.to_owned()).or_default();
            entry.retain(|(c, _)| *c != conn);
            entry.push((conn, template.clone()));
            Outcome::Ok(json!({}))
        }
        "objects/list" => {
            let names: Vec<&String> = st.objects.keys().collect();
            Outcome::Ok(json!({"objects": names}))
        }
        "objects/query" | "objects/subscribe" => {
            let Some(Value::Object(requested)) = params.get("objects") else {
                return Outcome::Error("Invalid argument".into());
            };
            if !valid_query(requested) {
                return Outcome::Error("Invalid argument".into());
            }
            let status = query(&st.objects, requested);
            if method == "objects/subscribe" {
                let template = params.get("response_template").and_then(Value::as_object).cloned().unwrap_or_default();
                if let Some(c) = st.conns.get_mut(&conn) {
                    c.subscription = Some((requested.clone(), template));
                }
            }
            Outcome::Ok(json!({"eventtime": now, "status": status}))
        }
        "gcode/help" => Outcome::Ok(Value::Object(st.script.capture.gcode_help.clone())),
        "gcode/subscribe_output" => {
            let template = params.get("response_template").and_then(Value::as_object).cloned().unwrap_or_default();
            if let Some(c) = st.conns.get_mut(&conn) {
                c.gcode_template = Some(template);
            }
            Outcome::Ok(json!({}))
        }
        "gcode/script" => {
            let Some(script) = params.get("script").and_then(Value::as_str) else {
                return Outcome::Error("Invalid argument".into());
            };
            run_gcode(st, script, now)
        }
        "gcode/restart" | "gcode/firmware_restart" => Outcome::Restart,
        "pause_resume/pause" => {
            pause_resume(st, true, "paused", now);
            Outcome::Ok(json!({}))
        }
        "pause_resume/resume" => {
            pause_resume(st, false, "printing", now);
            Outcome::Ok(json!({}))
        }
        "pause_resume/cancel" => {
            pause_resume(st, false, "cancelled", now);
            Outcome::Ok(json!({}))
        }
        // Other captured endpoints (bed_mesh/dump_mesh, query_endstops/status, ...) answer `{}`
        // unless a script gives them more behaviour.
        _ => Outcome::Ok(json!({})),
    }
}

fn info(st: &State) -> Value {
    let mut info = st.script.capture.info.clone();
    info.insert("state".into(), json!(st.state.as_str()));
    info.insert("state_message".into(), json!(st.state_message));
    info.insert("process_id".into(), json!(std::process::id()));
    Value::Object(info)
}

fn valid_query(requested: &Map<String, Value>) -> bool {
    requested.values().all(|v| match v {
        Value::Null => true,
        Value::Array(items) => items.iter().all(Value::is_string),
        _ => false,
    })
}

fn query(objects: &Map<String, Value>, requested: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, fields) in requested {
        let res = objects.get(name).and_then(Value::as_object);
        let selected = match (res, fields) {
            (None, _) => Map::new(),
            (Some(res), Value::Null) => res.clone(),
            (Some(res), Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(|f| (f.to_owned(), res.get(f).cloned().unwrap_or(Value::Null)))
                .collect(),
            (Some(_), _) => Map::new(),
        };
        out.insert(name.clone(), Value::Object(selected));
    }
    out
}

fn apply_patch(st: &mut State, patch: &Map<String, Value>, now: f64) {
    // Work out which fields really change before merging, as Klipper's diff does.
    let mut changed: Map<String, Value> = Map::new();
    for (name, fields) in patch {
        let Some(fields) = fields.as_object() else { continue };
        let current = st.objects.get(name).and_then(Value::as_object);
        let diff: Map<String, Value> = fields
            .iter()
            .filter(|(k, v)| current.and_then(|c| c.get(*k)) != Some(*v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !diff.is_empty() {
            changed.insert(name.clone(), Value::Object(diff));
        }
    }
    script::merge_objects(&mut st.objects, patch);
    if changed.is_empty() {
        return;
    }
    for conn in st.conns.values() {
        let Some((requested, template)) = &conn.subscription else { continue };
        let mut status = Map::new();
        for (name, fields) in &changed {
            let Some(wanted) = requested.get(name) else { continue };
            let fields = fields.as_object().expect("diff is built from objects");
            let picked: Map<String, Value> = match wanted {
                Value::Array(items) => fields
                    .iter()
                    .filter(|(k, _)| items.iter().any(|i| i.as_str() == Some(k.as_str())))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                _ => fields.clone(),
            };
            if !picked.is_empty() {
                status.insert(name.clone(), Value::Object(picked));
            }
        }
        if !status.is_empty() {
            let mut msg = template.clone();
            msg.insert("params".into(), json!({"eventtime": now, "status": status}));
            let _ = conn.tx.send(Value::Object(msg));
        }
    }
}

fn set_state(st: &mut State, state: KlippyState, message: Option<String>, now: f64) {
    st.state = state;
    st.state_message = message.unwrap_or_else(|| state.default_message().to_owned());
    let mut patch = Map::new();
    patch.insert("webhooks".into(), json!({"state": state.as_str(), "state_message": st.state_message}));
    apply_patch(st, &patch, now);
}

/// Drops all clients and enters `startup`. Returns what the caller needs to schedule the return
/// to the script's target state.
fn begin_restart(st: &mut State, now: f64) -> (u64, Duration, KlippyState) {
    st.generation += 1;
    st.conns.clear();
    st.remote_methods.clear();
    set_state(st, KlippyState::Startup, None, now);
    (st.generation, Duration::from_millis(st.script.restart_ms), st.script.state)
}

fn send_gcode_output(st: &State, line: &str) {
    for conn in st.conns.values() {
        if let Some(template) = &conn.gcode_template {
            let mut msg = template.clone();
            msg.insert("params".into(), json!({"response": line}));
            let _ = conn.tx.send(Value::Object(msg));
        }
    }
}

fn call_remote_method(st: &State, method: &str, params: Value) -> Result<(), String> {
    let Some(conns) = st.remote_methods.get(method).filter(|c| !c.is_empty()) else {
        return Err(format!("Remote method '{method}' not registered"));
    };
    for (conn, template) in conns {
        if let Some(c) = st.conns.get(conn) {
            let mut msg = template.clone();
            msg.insert("params".into(), params.clone());
            let _ = c.tx.send(Value::Object(msg));
        }
    }
    Ok(())
}

fn pause_resume(st: &mut State, paused: bool, print_state: &str, now: f64) {
    let mut patch = Map::new();
    patch.insert("pause_resume".into(), json!({"is_paused": paused}));
    patch.insert("print_stats".into(), json!({"state": print_state}));
    apply_patch(st, &patch, now);
}

fn run_gcode(st: &mut State, script: &str, now: f64) -> Outcome {
    if st.state != KlippyState::Ready {
        // Klipper refuses commands when not ready, except the few that recover it.
        let first = script.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        let cmd = first.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
        if !matches!(cmd.as_str(), "RESTART" | "FIRMWARE_RESTART" | "STATUS" | "HELP" | "ECHO") {
            return Outcome::Error(st.state_message.clone());
        }
    }
    for raw in script.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let cmd = line.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
        match cmd.as_str() {
            "RESTART" | "FIRMWARE_RESTART" => return Outcome::Restart,
            "M112" => {
                set_state(st, KlippyState::Shutdown, Some("Shutdown due to M112 command".into()), now);
                return Outcome::Error("Shutdown due to M112 command".into());
            }
            _ => {}
        }
        let rule = st.script.gcode.iter().find(|r| r.pattern.is_match(line)).cloned();
        let Some(rule) = rule else { continue };
        let caps = rule.pattern.captures(line).expect("rule matched");
        let vars: HashMap<&str, &str> =
            rule.pattern.capture_names().flatten().filter_map(|n| caps.name(n).map(|m| (n, m.as_str()))).collect();
        for out in &rule.respond {
            send_gcode_output(st, &substitute(out, &vars));
        }
        if !rule.set.is_empty() {
            let patch = substitute_value(&Value::Object(rule.set.clone()), &vars);
            apply_patch(st, patch.as_object().expect("set is an object"), now);
        }
        if let Some(err) = &rule.error {
            let msg = substitute(err, &vars);
            send_gcode_output(st, &format!("!! {msg}"));
            return Outcome::Error(msg);
        }
    }
    Outcome::Ok(json!({}))
}

fn substitute(s: &str, vars: &HashMap<&str, &str>) -> String {
    let mut out = s.to_owned();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

fn substitute_value(v: &Value, vars: &HashMap<&str, &str>) -> Value {
    match v {
        Value::String(s) => Value::String(substitute(s, vars)),
        Value::Array(items) => Value::Array(items.iter().map(|i| substitute_value(i, vars)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), substitute_value(v, vars))).collect()),
        other => other.clone(),
    }
}
