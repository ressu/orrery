//! The connection to Klippy's API socket, after Moonraker's `klippy_connection.py` and
//! `klippy_apis.py`.
//!
//! - Connects and reconnects, then runs Moonraker's startup sequence: `info` until Klippy leaves
//!   `startup`, subscribe to `webhooks` and gcode output, register Klippy's endpoints as
//!   `printer.*` methods, and check the required printer objects.
//! - Keeps one Klippy subscription that is the union of every client's, and sends each client
//!   only the fields it asked for.
//! - Serves the `printer.print.*` and restart endpoints that Moonraker builds on gcode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use crate::api::{ApiError, ApiResult, ConnId, Request, RequestType, Transport, Transports, handler};
use crate::server::{App, Event};
use orrery_proto::klippy as wire;

const INIT_TIME: Duration = Duration::from_millis(250);
/// Endpoints Moonraker uses itself and doesn't offer to clients.
/// Methods Klippy's macros can call with `action_call_remote_method`.
pub const REMOTE_METHODS: &[&str] = &["pause_job_queue", "start_job_queue"];
const RESERVED_ENDPOINTS: &[&str] = &["list_endpoints", "gcode/subscribe_output", "register_remote_method"];
/// Printer objects Moonraker needs for full function.
const REQUIRED_OBJECTS: &[&str] = &["virtual_sdcard", "display_status", "pause_resume"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KlippyState {
    Disconnected,
    Startup,
    Ready,
    Error,
    Shutdown,
}

impl KlippyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Startup => "startup",
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Shutdown => "shutdown",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "ready" => Self::Ready,
            "error" => Self::Error,
            "shutdown" => Self::Shutdown,
            _ => Self::Startup,
        }
    }
}

/// Who a subscription belongs to: a websocket client, or orrery itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subscriber {
    Conn(ConnId),
    Host,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, ApiError>>>;

struct Inner {
    writer: Option<mpsc::UnboundedSender<Vec<u8>>>,
    pending: Pending,
    next_id: u64,
    state: KlippyState,
    state_message: String,
    initializing: bool,
    missing_reqs: Vec<String>,
    /// Requested `{object: [fields] | null}` per subscriber.
    subscriptions: HashMap<Subscriber, Map<String, Value>>,
    /// Latest known fields of every subscribed object.
    cache: Map<String, Value>,
    /// The `[virtual_sdcard] path` from Klipper's config, once known.
    sdcard_path: Option<String>,
    /// The `log_file` from Klippy's `info`, once known.
    log_file: Option<String>,
}

pub struct Klippy {
    socket: PathBuf,
    inner: Mutex<Inner>,
    /// Serializes subscription changes, as Moonraker's `subscription_lock` does.
    subscription_lock: tokio::sync::Mutex<()>,
}

impl Klippy {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            inner: Mutex::new(Inner {
                writer: None,
                pending: HashMap::new(),
                next_id: 1,
                state: KlippyState::Disconnected,
                state_message: "Klippy Disconnected".into(),
                initializing: false,
                missing_reqs: Vec::new(),
                subscriptions: HashMap::new(),
                cache: Map::new(),
                sdcard_path: None,
                log_file: None,
            }),
            subscription_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("klippy lock")
    }

    /// True while orrery sets up a new Klippy connection, before it reports ready.
    pub fn is_initializing(&self) -> bool {
        self.lock().initializing
    }

    pub fn state(&self) -> (KlippyState, String) {
        let i = self.lock();
        (i.state, i.state_message.clone())
    }

    pub fn is_connected(&self) -> bool {
        self.lock().writer.is_some()
    }

    /// Klipper's log file, as Klippy reported it. Moonraker falls back to `/tmp/klippy.log`.
    /// Waits up to 20 seconds for a connected Klippy to finish starting, as Moonraker's
    /// `wait_started`. Returns whether Klippy is connected.
    pub async fn wait_started(&self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while self.is_connected() && self.lock().initializing && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.is_connected()
    }

    pub fn log_file(&self) -> PathBuf {
        PathBuf::from(self.lock().log_file.clone().unwrap_or_else(|| "/tmp/klippy.log".into()))
    }

    pub fn missing_requirements(&self) -> Vec<String> {
        self.lock().missing_reqs.clone()
    }

    /// A copy of the subscription cache for one object.
    pub fn cached(&self, object: &str) -> Option<Value> {
        self.lock().cache.get(object).cloned()
    }

    pub fn remove_subscription(&self, sub: Subscriber) {
        self.lock().subscriptions.remove(&sub);
    }

    /// Sends a request to Klippy. An empty result becomes `"ok"`, as in Moonraker.
    pub async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> ApiResult {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut i = self.lock();
            let Some(writer) = i.writer.clone() else {
                return Err(ApiError::new(503, "Klippy Host not connected"));
            };
            let id = i.next_id;
            i.next_id += 1;
            i.pending.insert(id, tx);
            let msg = json!({"id": id, "method": method, "params": params});
            if writer.send(wire::encode(&msg)).is_err() {
                i.pending.remove(&id);
                return Err(ApiError::new(503, "Klippy Disconnected"));
            }
            id
        };
        let outcome = match timeout {
            Some(t) => match tokio::time::timeout(t, rx).await {
                Ok(r) => r,
                Err(_) => {
                    self.lock().pending.remove(&id);
                    return Err(ApiError::new(500, format!("Klippy request {method} timed out")));
                }
            },
            None => rx.await,
        };
        outcome.unwrap_or_else(|_| Err(ApiError::new(503, "Klippy Disconnected")))
    }

    /// Subscribes `sub` to `objects`, replacing its previous subscription. Klippy gets the union
    /// of all subscriptions; the reply holds only what `sub` asked for.
    pub async fn subscribe(&self, app: &App, sub: Subscriber, objects: Map<String, Value>) -> ApiResult {
        self.subscribe_inner(app, sub, objects, false).await
    }

    /// Adds objects to orrery's own subscription, as `klippy_apis.subscribe_objects` does.
    pub async fn subscribe_host(&self, app: &App, objects: Map<String, Value>) -> ApiResult {
        self.subscribe_inner(app, Subscriber::Host, objects, true).await
    }

    /// With `merge`, `objects` is added to the subscriber's existing objects instead of replacing
    /// them. The merge happens under the lock, so two components subscribing at once both keep
    /// their objects.
    async fn subscribe_inner(&self, app: &App, sub: Subscriber, objects: Map<String, Value>, merge: bool) -> ApiResult {
        let _guard = self.subscription_lock.lock().await;
        let mut objects = objects;
        let union = {
            let mut i = self.lock();
            let previous = i.subscriptions.remove(&sub);
            if merge && let Some(mut prev) = previous {
                prev.extend(objects);
                objects = prev;
            }
            let mut all = objects.clone();
            for s in i.subscriptions.values() {
                for (obj, fields) in s {
                    let merged = match (all.get(obj), fields) {
                        (None, f) => f.clone(),
                        (Some(Value::Null), _) | (Some(_), Value::Null) => Value::Null,
                        (Some(Value::Array(a)), Value::Array(b)) => {
                            let mut u = a.clone();
                            u.extend(b.iter().filter(|x| !a.contains(x)).cloned());
                            Value::Array(u)
                        }
                        (Some(a), _) => a.clone(),
                    };
                    all.insert(obj.clone(), merged);
                }
            }
            all
        };
        let params = json!({"objects": union, "response_template": {"method": "process_status_update"}});
        let mut result = self.request("objects/subscribe", params, Some(Duration::from_secs(20))).await?;
        let eventtime = result["eventtime"].as_f64().unwrap_or_default();
        let all_status = result.get("status").and_then(Value::as_object).cloned().unwrap_or_default();

        let mut pruned = Map::new();
        let mut diff = Map::new();
        {
            let mut i = self.lock();
            for (obj, fields) in &all_status {
                let fields = fields.as_object().cloned().unwrap_or_default();
                if let Some(Value::Object(cached)) = i.cache.get(obj) {
                    let changed: Map<String, Value> = fields
                        .iter()
                        .filter(|(k, v)| cached.get(*k).is_some_and(|c| c != *v))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if !changed.is_empty() {
                        diff.insert(obj.clone(), Value::Object(changed));
                    }
                }
                let mut to_cache = fields.clone();
                if obj == "configfile" {
                    // Large and never changing, so Moonraker doesn't cache them.
                    to_cache.remove("config");
                    to_cache.remove("settings");
                }
                i.cache.insert(obj.clone(), Value::Object(to_cache));
                match objects.get(obj) {
                    Some(Value::Array(wanted)) => {
                        let picked: Map<String, Value> =
                            fields.into_iter().filter(|(k, _)| wanted.iter().any(|w| w.as_str() == Some(k))).collect();
                        pruned.insert(obj.clone(), Value::Object(picked));
                    }
                    Some(_) => {
                        pruned.insert(obj.clone(), Value::Object(fields));
                    }
                    None => {}
                }
            }
            i.cache.retain(|k, _| all_status.contains_key(k));
        }
        if !diff.is_empty() {
            // The reply shows changes the other subscribers haven't seen yet.
            self.process_status_update(app, eventtime, diff);
        }
        if !objects.is_empty() {
            self.lock().subscriptions.insert(sub, objects);
        }
        result["status"] = Value::Object(pruned);
        Ok(result)
    }

    fn process_status_update(&self, app: &App, eventtime: f64, status: Map<String, Value>) {
        let mut shutdown = false;
        let deliveries: Vec<(Subscriber, Map<String, Value>)> = {
            let mut i = self.lock();
            for (obj, fields) in &status {
                let entry = i.cache.entry(obj.clone()).or_insert_with(|| Value::Object(Map::new()));
                if let (Value::Object(dst), Value::Object(src)) = (entry, fields) {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
            }
            if let Some(wh) = status.get("webhooks") {
                if let Some(msg) = wh.get("state_message").and_then(Value::as_str) {
                    i.state_message = msg.to_owned();
                }
                if let Some(s) = wh.get("state").and_then(Value::as_str) {
                    let new_state = KlippyState::parse(s);
                    if new_state == KlippyState::Shutdown && !i.initializing && i.state != KlippyState::Shutdown {
                        tracing::info!("Klippy has shutdown");
                        shutdown = true;
                    }
                    i.state = new_state;
                }
            }
            i.subscriptions
                .iter()
                .map(|(sub, wanted)| {
                    let mut out = Map::new();
                    for (obj, fields) in &status {
                        let Some(want) = wanted.get(obj) else { continue };
                        let Value::Object(fields) = fields else { continue };
                        let picked: Map<String, Value> = match want {
                            Value::Array(w) => fields
                                .iter()
                                .filter(|(k, _)| w.iter().any(|x| x.as_str() == Some(k.as_str())))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                            _ => fields.clone(),
                        };
                        if !picked.is_empty() {
                            out.insert(obj.clone(), Value::Object(picked));
                        }
                    }
                    (*sub, out)
                })
                .filter(|(_, s)| !s.is_empty())
                .collect()
        };
        if shutdown {
            app.send_event(Event::KlippyShutdown);
        }
        for (sub, status) in deliveries {
            match sub {
                Subscriber::Conn(id) => app.send_to(
                    id,
                    &json!({"jsonrpc": "2.0", "method": "notify_status_update", "params": [status, eventtime]}),
                ),
                Subscriber::Host => app.send_event(Event::HostStatus(status)),
            }
        }
    }
}

/// Connects to Klippy, initializes, and reconnects whenever the connection drops.
pub async fn run(app: Arc<App>) {
    loop {
        let stream = loop {
            match UnixStream::connect(&app.klippy.socket).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(INIT_TIME).await,
            }
        };
        tracing::info!("Klippy Connection Established");
        let (read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        {
            let mut i = app.klippy.lock();
            i.writer = Some(tx);
            i.state = KlippyState::Startup;
            i.initializing = true;
        }
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        let init = tokio::spawn(initialize(app.clone()));
        read_loop(&app, read).await;
        init.abort();
        writer.abort();
        on_disconnect(&app);
    }
}

async fn read_loop(app: &Arc<App>, read: tokio::net::unix::OwnedReadHalf) {
    let mut reader = BufReader::new(read);
    loop {
        match wire::read_message(&mut reader).await {
            Ok(Some(msg)) => handle_message(app, msg),
            Ok(None) => return,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                tracing::warn!("Klippy sent a message that isn't JSON: {e}");
            }
            Err(e) => {
                tracing::info!("Klippy connection error: {e}");
                return;
            }
        }
    }
}

/// Asks Klippy to send calls to `method` (from `action_call_remote_method`) to orrery.
pub async fn register_remote_method(app: &Arc<App>, method: &str) {
    let params = json!({"response_template": {"method": method}, "remote_method": method});
    if let Err(e) = app.klippy.request("register_remote_method", params, None).await {
        tracing::warn!("Unable to register method '{method}': {e}");
    }
}

fn handle_message(app: &Arc<App>, msg: Value) {
    if let Some(method) = msg.get("method").and_then(Value::as_str) {
        let params = msg.get("params").cloned().unwrap_or_default();
        match method {
            "process_status_update" => {
                let eventtime = params["eventtime"].as_f64().unwrap_or_default();
                let status = params.get("status").and_then(Value::as_object).cloned().unwrap_or_default();
                app.klippy.process_status_update(app, eventtime, status);
            }
            "process_gcode_response" => {
                if let Some(line) = params.get("response").and_then(Value::as_str) {
                    app.send_event(Event::GcodeResponse(line.to_owned()));
                }
            }
            "pause_job_queue" => {
                tokio::spawn(crate::job_queue::pause_queue(app.clone()));
            }
            "start_job_queue" => {
                tokio::spawn(crate::job_queue::start_queue(app.clone()));
            }
            other => match app.extensions.method_conn(other) {
                Some(conn) => crate::extensions::forward_remote_method(app, conn, other, &params),
                None => tracing::info!("Unknown method received: {other}"),
            },
        }
        return;
    }
    let Some(id) = msg.get("id").and_then(Value::as_u64) else { return };
    let Some(tx) = app.klippy.lock().pending.remove(&id) else {
        tracing::info!("No request matching request ID: {id}");
        return;
    };
    let outcome = match msg.get("result") {
        Some(result) if is_falsy(result) => Ok(json!("ok")),
        Some(result) => Ok(result.clone()),
        None => {
            let err = match msg.get("error") {
                Some(Value::Object(e)) => {
                    e.get("message").and_then(Value::as_str).unwrap_or("Malformed Klippy Response")
                }
                Some(Value::String(s)) => s.as_str(),
                _ => "Malformed Klippy Response",
            };
            Err(ApiError::bad_request(err))
        }
    };
    let _ = tx.send(outcome);
}

/// Python truthiness, which Moonraker uses to turn empty results into `"ok"`.
fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn on_disconnect(app: &Arc<App>) {
    let pending = {
        let mut i = app.klippy.lock();
        i.writer = None;
        i.state = KlippyState::Disconnected;
        i.state_message = "Klippy Disconnected".into();
        i.initializing = false;
        i.subscriptions.clear();
        i.cache.clear();
        i.missing_reqs.clear();
        std::mem::take(&mut i.pending)
    };
    for (_, tx) in pending {
        let _ = tx.send(Err(ApiError::new(503, "Klippy Disconnected")));
    }
    tracing::info!("Klippy Connection Removed");
    app.send_event(Event::KlippyDisconnected);
}

/// Moonraker's `_init_klippy_connection` and `_check_ready`.
async fn initialize(app: Arc<App>) {
    let mut identified = false;
    loop {
        tokio::time::sleep(INIT_TIME).await;
        let params = if identified {
            json!({})
        } else {
            json!({"client_info": {"program": "orrery", "version": env!("CARGO_PKG_VERSION")}})
        };
        let Ok(info) = app.klippy.request("info", params, None).await else { continue };
        if let Some(msg) = info.get("state_message").and_then(Value::as_str) {
            app.klippy.lock().state_message = msg.to_owned();
        }
        if let Some(log_file) = info.get("log_file").and_then(Value::as_str) {
            app.klippy.lock().log_file = Some(log_file.to_owned());
        }
        let Some(state) = info.get("state").and_then(Value::as_str) else { continue };
        if !identified {
            identified = true;
            register_klippy_endpoints(&app).await;
        }
        let state = KlippyState::parse(state);
        app.klippy.lock().state = state;
        if state == KlippyState::Startup {
            continue;
        }
        let mut webhooks = Map::new();
        webhooks.insert("webhooks".into(), Value::Null);
        if let Err(e) = app.klippy.subscribe_host(&app, webhooks).await {
            tracing::warn!("Unable to subscribe to webhooks object: {e}");
        }
        let template = json!({"response_template": {"method": "process_gcode_response"}});
        if let Err(e) = app.klippy.request("gcode/subscribe_output", template, None).await {
            tracing::warn!("Unable to register gcode output subscription: {e}");
        }
        register_klippy_endpoints(&app).await;
        let current = app.klippy.state().0;
        if current == KlippyState::Ready {
            verify_requirements(&app).await;
            let agent_methods = app.extensions.remote_methods();
            for method in REMOTE_METHODS.iter().copied().chain(agent_methods.iter().map(String::as_str)) {
                register_remote_method(&app, method).await;
            }
            app.klippy.lock().initializing = false;
            tracing::info!("Klippy ready");
            app.send_event(Event::KlippyReady);
            if app.klippy.state().0 == KlippyState::Shutdown {
                app.send_event(Event::KlippyShutdown);
            }
        } else {
            let (s, msg) = app.klippy.state();
            app.klippy.lock().initializing = false;
            tracing::info!("Klippy state {}: {msg}", s.as_str());
            if s == KlippyState::Shutdown && state != KlippyState::Shutdown {
                app.send_event(Event::KlippyShutdown);
            }
        }
        return;
    }
}

async fn register_klippy_endpoints(app: &Arc<App>) {
    let Ok(result) = app.klippy.request("list_endpoints", json!({}), None).await else { return };
    let endpoints = result.get("endpoints").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut registry = app.registry.write().expect("registry lock");
    for ep in endpoints.iter().filter_map(Value::as_str) {
        if !RESERVED_ENDPOINTS.contains(&ep) {
            registry.register_remote(ep, handler(remote_request));
        }
    }
}

async fn verify_requirements(app: &Arc<App>) {
    let Ok(list) = app.klippy.request("objects/list", json!({}), None).await else {
        tracing::info!("Unable to retrieve Klipper Object List");
        return;
    };
    let objects: Vec<&str> =
        list["objects"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let missing: Vec<String> =
        REQUIRED_OBJECTS.iter().filter(|o| !objects.contains(o)).map(|o| o.to_string()).collect();
    if !missing.is_empty() {
        app.add_warning(format!(
            "Unable to detect these printer objects: {}. Add the sections to printer.cfg for full function.",
            missing.iter().map(|m| format!("[{m}]")).collect::<Vec<_>>().join(", ")
        ));
    }
    let has_vsd = !missing.iter().any(|m| m == "virtual_sdcard");
    app.klippy.lock().missing_reqs = missing;
    if has_vsd
        && let Ok(res) = app.klippy.request("objects/query", json!({"objects": {"configfile": null}}), None).await
        && let Some(path) = res["status"]["configfile"]["config"]["virtual_sdcard"]["path"].as_str()
    {
        app.klippy.lock().sdcard_path = Some(path.to_owned());
    }
}

/// Handles a `printer.*` method that Klippy provides.
async fn remote_request(app: Arc<App>, req: Request) -> ApiResult {
    if !app.klippy.is_connected() {
        return Err(ApiError::new(503, "Klippy Host not connected"));
    }
    if req.endpoint == "objects/subscribe" {
        let Some(conn) = req.conn.filter(|_| req.transport == Transport::Websocket) else {
            return Err(ApiError::bad_request("No connection associated with subscription request"));
        };
        let objects = match req.args.get("objects") {
            Some(Value::Object(m)) => m.clone(),
            None => Map::new(),
            Some(_) => return Err(ApiError::bad_request("Invalid argument")),
        };
        return app.klippy.subscribe(&app, Subscriber::Conn(conn), objects).await;
    }
    if req.endpoint == "gcode/script"
        && let Some(script) = req.args.get("script").and_then(Value::as_str)
        && !script.is_empty()
    {
        app.send_event(Event::GcodeReceived(script.to_owned()));
    }
    app.klippy.request(&req.endpoint, Value::Object(req.args.0), None).await
}

/// Runs a gcode script through Klippy, as `klippy_apis.run_gcode` does.
pub async fn run_gcode(app: &Arc<App>, script: &str) -> ApiResult {
    app.send_event(Event::GcodeReceived(script.to_owned()));
    app.klippy.request("gcode/script", json!({"script": script}), None).await
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    let post = &[RequestType::Post];
    r.register("/printer/print/pause", post, Transports::All, handler(|app, _| klippy_call(app, "pause_resume/pause")));
    r.register(
        "/printer/print/resume",
        post,
        Transports::All,
        handler(|app, _| klippy_call(app, "pause_resume/resume")),
    );
    r.register(
        "/printer/print/cancel",
        post,
        Transports::All,
        handler(|app, _| klippy_call(app, "pause_resume/cancel")),
    );
    r.register("/printer/print/start", post, Transports::All, handler(print_start));
    r.register("/printer/restart", post, Transports::All, handler(|app, _| restart(app, "RESTART")));
    r.register("/printer/firmware_restart", post, Transports::All, handler(|app, _| restart(app, "FIRMWARE_RESTART")));
}

async fn klippy_call(app: Arc<App>, endpoint: &'static str) -> ApiResult {
    app.klippy.request(endpoint, json!({}), None).await
}

async fn print_start(app: Arc<App>, req: Request) -> ApiResult {
    let filename = req.args.str("filename")?;
    start_print(&app, &filename).await
}

/// Starts printing a file from the gcodes folder.
pub async fn start_print(app: &Arc<App>, filename: &str) -> ApiResult {
    let name = filename.strip_prefix('/').unwrap_or(filename).replace('"', "\\\"");
    tracing::info!("Requesting Job Start, filename = {name}");
    let result = run_gcode(app, &format!("SDCARD_PRINT_FILE FILENAME=\"{name}\"")).await;
    // Every client is trusted until orrery has authorization.
    crate::history::job_requested(app, crate::access::TRUSTED_USER).await;
    result
}

async fn restart(app: Arc<App>, gcode: &str) -> ApiResult {
    match run_gcode(&app, gcode).await {
        Err(e) if e.message == "Klippy Disconnected" => Ok(json!("ok")),
        other => other,
    }
}
