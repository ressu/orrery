//! The application: shared state, the event bus, websocket clients, JSON-RPC dispatch, and the
//! `server.*` endpoints that don't belong to a component.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::api::{ApiError, ApiResult, Args, ConnId, Registry, Request, RequestType, Transport, Transports, handler};
use crate::config::Config;
use crate::data_store::DataStore;
use crate::database::Database;
use crate::files::FileManager;
use crate::job_state::JobState;
use crate::klippy::Klippy;

/// The API version orrery reports. It's Moonraker's, since orrery serves the same API.
pub const API_VERSION: [u32; 3] = [1, 5, 0];

/// Where orrery keeps its files, from `-d`, `-c`, and `-l`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub config_file: PathBuf,
    /// orrery's own log file, or `None` with `-n`.
    pub log_file: Option<PathBuf>,
}

impl Paths {
    pub fn config_dir(&self) -> PathBuf {
        self.data.join("config")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.data.join("logs")
    }
    pub fn gcodes_dir(&self) -> PathBuf {
        self.data.join("gcodes")
    }
    pub fn database_dir(&self) -> PathBuf {
        self.data.join("database")
    }
    pub fn backup_dir(&self) -> PathBuf {
        self.data.join("backup/database")
    }
}

/// Internal events, after Moonraker's `server.send_event`.
#[derive(Debug, Clone)]
pub enum Event {
    KlippyReady,
    KlippyShutdown,
    KlippyDisconnected,
    /// A line of gcode output from Klippy.
    GcodeResponse(String),
    /// A gcode script sent to Klippy.
    GcodeReceived(String),
    /// Changed fields for orrery's own subscription ([`crate::klippy::Subscriber::Host`]).
    HostStatus(Map<String, Value>),
}

struct Client {
    tx: mpsc::UnboundedSender<String>,
    identity: Option<Value>,
    peer: Peer,
    access: crate::auth::Access,
}

/// What orrery knows about the other end of a connection.
#[derive(Debug, Clone, Copy, Default)]
pub struct Peer {
    /// Connected over the unix socket.
    pub unix: bool,
    /// The connecting process's user id, from the unix socket.
    pub uid: Option<u32>,
    /// The connecting process's id, from the unix socket.
    pub pid: Option<u32>,
}

pub struct App {
    pub config: Config,
    pub paths: Paths,
    pub registry: RwLock<Registry>,
    clients: Mutex<HashMap<ConnId, Client>>,
    next_conn: AtomicU64,
    pub events: broadcast::Sender<Event>,
    pub klippy: Klippy,
    pub database: Database,
    pub files: FileManager,
    pub data_store: DataStore,
    pub job_state: JobState,
    pub job_queue: crate::job_queue::JobQueue,
    pub machine: crate::machine::Machine,
    pub proc_stats: crate::proc_stats::ProcStats,
    pub webcams: crate::webcam::Webcams,
    pub history: crate::history::History,
    pub cors: crate::access::Cors,
    pub auth: crate::auth::Auth,
    pub keys: crate::keys::Keys,
    pub extensions: crate::extensions::Extensions,
    pub agents: crate::agents::Agents,
    warnings: Mutex<Vec<String>>,
    /// Set when a client asks orrery to restart; `main` waits for it beside the stop signals.
    pub restart: tokio::sync::Notify,
}

/// Moonraker's names for the components orrery implements, as `server.info` lists them.
const COMPONENTS: &[&str] = &[
    "application",
    "websockets",
    "klippy_connection",
    "database",
    "file_manager",
    "klippy_apis",
    "machine",
    "data_store",
    "proc_stats",
    "job_state",
    "job_queue",
    "history",
    "webcam",
    "extensions",
];

impl App {
    pub fn new(config: Config, paths: Paths) -> Result<Arc<Self>> {
        let (events, _) = broadcast::channel(1024);
        let server = config.section("server");
        let socket = server.get_str("klippy_uds_address", &paths.data.join("comms/klippy.sock").to_string_lossy());
        config.record_moonraker_options()?;
        let database = Database::open(&config, &paths)?;
        let files = FileManager::new(&config, &paths)?;
        let machine = crate::machine::Machine::new(&config, &paths)?;
        let data_store = DataStore::new(&config)?;
        let webcams = crate::webcam::Webcams::new(&config, &paths)?;
        let job_queue = crate::job_queue::JobQueue::new(&config)?;
        let (auth, mut warnings) = crate::auth::Auth::new(&config)?;
        let (cors, cors_warnings) = crate::access::Cors::new(&config, auth.trusted_networks());
        warnings.extend(cors_warnings);
        auth.log_settings();
        let exe = std::env::current_exe().ok();
        let (agents, agent_warnings) = crate::agents::Agents::new(&config, exe.as_deref().and_then(Path::parent))?;
        for w in &agent_warnings {
            tracing::warn!("{w}");
        }
        warnings.extend(agent_warnings);
        let keys = crate::keys::Keys::new(&config, &agents, &database)?;
        keys.log_settings();
        // Connection ids look like Moonraker's (Python object ids): large numbers.
        let base = 0x7f00_0000_0000 + (std::process::id() as u64) * 0x1000;
        let app = Arc::new(Self {
            klippy: Klippy::new(PathBuf::from(socket)),
            config,
            paths,
            registry: RwLock::new(Registry::default()),
            clients: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(base),
            events,
            database,
            files,
            data_store,
            job_state: JobState::default(),
            job_queue,
            machine,
            proc_stats: crate::proc_stats::ProcStats::default(),
            webcams,
            history: crate::history::History::default(),
            cors,
            auth,
            keys,
            extensions: crate::extensions::Extensions::default(),
            agents,
            warnings: Mutex::new(warnings),
            restart: tokio::sync::Notify::new(),
        });
        app.register_endpoints();
        crate::klippy::register_endpoints(&app);
        crate::database::register_endpoints(&app);
        crate::data_store::register_endpoints(&app);
        crate::files::register_endpoints(&app);
        crate::access::register_endpoints(&app);
        crate::machine::register_endpoints(&app);
        crate::proc_stats::register_endpoints(&app);
        crate::job_queue::register_endpoints(&app);
        crate::webcam::register_endpoints(&app);
        crate::extensions::register_endpoints(&app);
        crate::agents::register_endpoints(&app);
        crate::history::register_endpoints(&app);
        // Last, so it sees every built-in method when it checks the maps.
        crate::agents::start(&app);
        Ok(app)
    }

    /// Starts the background tasks: the Klippy connection, notifications, and stores.
    pub fn start(self: &Arc<Self>) {
        tokio::spawn(crate::klippy::run(self.clone()));
        tokio::spawn(forward_notifications(self.clone()));
        crate::data_store::start(self);
        crate::files::start(self);
        crate::history::start(self);
        crate::job_state::start(self);
        crate::job_queue::start(self);
        crate::machine::start(self);
        crate::proc_stats::start(self);
        crate::webcam::start(self);
    }

    pub fn add_warning(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::warn!("{message}");
        self.warnings.lock().expect("warnings lock").push(message);
    }

    pub fn send_event(&self, event: Event) {
        // An error only means nobody is listening yet.
        let _ = self.events.send(event);
    }

    // *** Websocket clients ***

    pub fn add_client(&self, tx: mpsc::UnboundedSender<String>, peer: Peer, access: crate::auth::Access) -> ConnId {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().expect("clients lock").insert(id, Client { tx, identity: None, peer, access });
        id
    }

    /// What a connection may do.
    pub fn client_access(&self, id: ConnId) -> crate::auth::Access {
        self.clients.lock().expect("clients lock").get(&id).map(|c| c.access.clone()).unwrap_or_default()
    }

    /// Takes away the rights a key gave, from every connection that used it: after a rotation.
    pub fn revoke_key(&self, name: &str) {
        for (id, c) in self.clients.lock().expect("clients lock").iter_mut() {
            if c.access.key.as_deref() == Some(name) {
                tracing::info!(target: "orrery::auth", "Connection {id} loses the rights of key {name}, which was rotated");
                c.access = crate::auth::Access::none(format!("key {name} was rotated"));
            }
        }
    }

    /// Gives a connection new rights: from a key it sent, or an agent name it claimed.
    pub fn set_client_access(&self, id: ConnId, access: crate::auth::Access) {
        if let Some(c) = self.clients.lock().expect("clients lock").get_mut(&id) {
            c.access = access;
        }
    }

    pub fn client_peer(&self, id: ConnId) -> Peer {
        self.clients.lock().expect("clients lock").get(&id).map(|c| c.peer).unwrap_or_default()
    }

    pub fn remove_client(&self, id: ConnId) {
        crate::extensions::remove_connection(self, id);
        self.clients.lock().expect("clients lock").remove(&id);
        self.klippy.remove_subscription(crate::klippy::Subscriber::Conn(id));
    }

    /// What a client sent in `server.connection.identify`: name, version, type, and url.
    pub fn client_identity(&self, id: ConnId) -> Option<Value> {
        self.clients.lock().expect("clients lock").get(&id)?.identity.clone()
    }

    /// The name a client gave in `server.connection.identify`.
    pub fn client_name(&self, id: ConnId) -> Option<String> {
        let clients = self.clients.lock().expect("clients lock");
        let identity = clients.get(&id)?.identity.as_ref()?;
        identity["name"].as_str().map(str::to_owned)
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().expect("clients lock").len()
    }

    pub fn send_to(&self, id: ConnId, message: &Value) {
        if let Some(c) = self.clients.lock().expect("clients lock").get(&id) {
            let _ = c.tx.send(message.to_string());
        }
    }

    /// Sends `notify_<name>` to every client. `params` is left out when it's empty, as in
    /// Moonraker.
    pub fn notify(&self, name: &str, params: Vec<Value>) {
        let mut msg = json!({"jsonrpc": "2.0", "method": format!("notify_{name}")});
        if !params.is_empty() {
            msg["params"] = Value::Array(params);
        }
        let text = msg.to_string();
        for c in self.clients.lock().expect("clients lock").values() {
            let _ = c.tx.send(text.clone());
        }
    }

    /// Sends `notify_<name>` to every client except `except`, as Moonraker does for agent
    /// events.
    pub fn notify_except(&self, name: &str, params: Vec<Value>, except: ConnId) {
        let mut msg = json!({"jsonrpc": "2.0", "method": format!("notify_{name}")});
        if !params.is_empty() {
            msg["params"] = Value::Array(params);
        }
        let text = msg.to_string();
        for (id, c) in self.clients.lock().expect("clients lock").iter() {
            if *id != except {
                let _ = c.tx.send(text.clone());
            }
        }
    }

    // *** Dispatch ***

    /// Runs a JSON-RPC message (one request or a batch) and returns the reply, if any.
    pub async fn dispatch_jsonrpc(self: &Arc<Self>, text: &str, conn: Option<ConnId>) -> Option<Value> {
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return Some(rpc_error(-32700, "Parse error", Value::Null)),
        };
        match parsed {
            Value::Array(items) => {
                let mut out = Vec::new();
                for item in items {
                    if let Some(r) = self.process_rpc(item, conn).await {
                        out.push(r);
                    }
                }
                (!out.is_empty()).then_some(Value::Array(out))
            }
            obj => self.process_rpc(obj, conn).await,
        }
    }

    async fn process_rpc(self: &Arc<Self>, obj: Value, conn: Option<ConnId>) -> Option<Value> {
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(-32600, "Invalid Request", id));
        }
        // A message without a method is a client's reply to a call orrery sent it.
        let Some(method) = obj.get("method") else {
            if let Some(conn) = conn {
                crate::extensions::resolve_response(self, conn, &obj);
            }
            return None;
        };
        let Some(method) = method.as_str() else {
            return Some(rpc_error(-32600, "Invalid Request", id));
        };
        let found = self.registry.read().expect("registry lock").by_method(method);
        let Some((endpoint, kind)) = found else {
            return Some(rpc_error(-32601, "Method not found", id));
        };
        let args = match obj.get("params") {
            None => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Some(rpc_error(-32602, "Invalid params:", id)),
        };
        let args = Args(args);
        let access = conn.map(|c| self.client_access(c)).unwrap_or_default();
        let result = match crate::auth::authorize(self, &access, &endpoint.endpoint, kind, &args) {
            Ok(()) => {
                let req = Request {
                    endpoint: endpoint.endpoint.clone(),
                    kind,
                    args,
                    transport: Transport::Websocket,
                    conn,
                    access,
                };
                (endpoint.handler)(self.clone(), req).await
            }
            Err(e) => {
                self.auth.log_refusal(&access, method, &e);
                Err(e)
            }
        };
        if id.is_null() {
            return None;
        }
        Some(match result {
            Ok(v) => json!({"jsonrpc": "2.0", "result": v, "id": id}),
            Err(e) => {
                tracing::info!("JSON-RPC Request Error - Requested Method: {method}, Code: {}, Message: {e}", e.status);
                let mut reply = rpc_error(e.jsonrpc_code(), &e.message, id);
                if let Some(data) = e.data {
                    reply["error"]["data"] = data;
                }
                reply
            }
        })
    }

    fn register_endpoints(self: &Arc<Self>) {
        let mut r = self.registry.write().expect("registry lock");
        r.register("/server/info", &[RequestType::Get], Transports::All, handler(server_info));
        r.register("/server/config", &[RequestType::Get], Transports::All, handler(server_config));
        r.register(
            "/server/connection/identify",
            &[RequestType::Post],
            Transports::WebsocketOnly,
            handler(connection_identify),
        );
        r.register("/server/websocket/id", &[RequestType::Get], Transports::WebsocketOnly, handler(websocket_id));
        r.register("/server/logs/rollover", &[RequestType::Post], Transports::All, handler(logs_rollover));
        r.register("/server/restart", &[RequestType::Post], Transports::All, handler(|app, _| server_restart(app)));
    }
}

/// The end of a "Closed" log line: who the client was and how long it stayed.
pub fn describe_close(name: Option<String>, opened: std::time::Instant) -> String {
    let secs = opened.elapsed().as_secs();
    let open_for = match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    };
    format!("Name: {}, Open For: {open_for}", name.as_deref().unwrap_or("(not identified)"))
}

fn rpc_error(code: i64, message: &str, id: Value) -> Value {
    json!({"jsonrpc": "2.0", "error": {"code": code, "message": message}, "id": id})
}

/// Turns internal events into the notifications Moonraker sends for them.
async fn forward_notifications(app: Arc<App>) {
    let mut rx = app.events.subscribe();
    loop {
        match rx.recv().await {
            Ok(Event::KlippyReady) => app.notify("klippy_ready", vec![]),
            Ok(Event::KlippyShutdown) => app.notify("klippy_shutdown", vec![]),
            Ok(Event::KlippyDisconnected) => app.notify("klippy_disconnected", vec![]),
            Ok(Event::GcodeResponse(line)) => app.notify("gcode_response", vec![json!(line)]),
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!("notifications fell behind by {n} events"),
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn server_info(app: Arc<App>, req: Request) -> ApiResult {
    let raw = req.args.opt_bool("raw")?.unwrap_or(false);
    let warnings: Vec<String> = app
        .warnings
        .lock()
        .expect("warnings lock")
        .iter()
        .map(|w| if raw { w.clone() } else { w.replace('\n', "<br/>") })
        .collect();
    let state = app.klippy.state();
    Ok(json!({
        "klippy_connected": app.klippy.is_connected(),
        "klippy_state": state.0.as_str(),
        "components": COMPONENTS.iter().copied().chain(app.agents.components()).collect::<Vec<_>>(),
        "failed_components": [],
        "registered_directories": app.files.root_names(),
        "warnings": warnings,
        "websocket_count": app.client_count(),
        "moonraker_version": concat!("orrery-", env!("CARGO_PKG_VERSION")),
        "missing_klippy_requirements": app.klippy.missing_requirements(),
        "api_version": API_VERSION,
        "api_version_string": API_VERSION.map(|v| v.to_string()).join("."),
    }))
}

async fn server_config(app: Arc<App>, _req: Request) -> ApiResult {
    Ok(json!({"config": app.config.parsed(), "orig": app.config.orig(), "files": app.config.files()}))
}

/// Restarts orrery, as Moonraker's `server.restart`: it answers first, then shuts down as for
/// SIGTERM and starts its program again in the same process.
async fn server_restart(app: Arc<App>) -> ApiResult {
    tracing::info!("Restarting, as a client asked");
    tokio::spawn(async move {
        // Time for the answer to reach the client before the connections close.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        app.restart.notify_one();
    });
    Ok(json!("ok"))
}

/// Starts a new log file for orrery, Klipper, or both. Klipper's needs Klipper stopped; see
/// [`crate::services::rollover_klipper_log`].
async fn logs_rollover(app: Arc<App>, req: Request) -> ApiResult {
    let mut apps = vec!["moonraker", "klipper"];
    if let Some(app) = req.args.opt_str("application")? {
        if !apps.contains(&app.as_str()) {
            return Err(ApiError::bad_request(format!("Unknown application {app}")));
        }
        apps.retain(|a| *a == app);
    }
    let (mut rolled_over, mut failed) = (Vec::new(), Map::new());
    if apps.contains(&"moonraker") {
        match tokio::task::spawn_blocking(crate::logfile::rollover).await {
            Ok(Some(Ok(()))) => {
                tracing::info!("Started a new log file, as a client asked");
                rolled_over.push("moonraker");
            }
            Ok(Some(Err(e))) => drop(failed.insert("moonraker".into(), json!(e.to_string()))),
            Ok(None) => drop(failed.insert("moonraker".into(), json!("File Logging Disabled"))),
            Err(e) => drop(failed.insert("moonraker".into(), json!(e.to_string()))),
        }
    }
    if apps.contains(&"klipper") {
        match crate::services::rollover_klipper_log(&app).await {
            Ok(()) => rolled_over.push("klipper"),
            Err(e) => drop(failed.insert("klipper".into(), json!(e))),
        }
    }
    Ok(json!({"rolled_over": rolled_over, "failed": failed}))
}

const CLIENT_TYPES: &[&str] = &["web", "mobile", "desktop", "display", "bot", "agent", "other"];

async fn connection_identify(app: Arc<App>, req: Request) -> ApiResult {
    let conn = req.conn.ok_or_else(|| ApiError::bad_request("No connection associated with request"))?;
    let name = req.args.str("client_name")?;
    let version = req.args.str("version")?;
    let client_type = req.args.str("type")?.to_ascii_lowercase();
    let url = req.args.str("url")?;
    if !CLIENT_TYPES.contains(&client_type.as_str()) {
        return Err(ApiError::bad_request(format!("Invalid Client Type: {client_type}")));
    }
    // A key sent here gives the connection the key's rights, as Moonraker's `authenticate`.
    if let Some(key) = req.args.opt_str("api_key")?.filter(|k| !k.trim().is_empty()) {
        match app.keys.lookup(&key) {
            Some(access) => {
                tracing::info!(target: "orrery::auth", "Connection {conn} identified with a key: {}", access.describe());
                app.set_client_access(conn, access);
            }
            None => {
                let e = ApiError::new(401, "Invalid API Key");
                app.auth.log_refusal(&req.access, "server.connection.identify", &e);
                return Err(e);
            }
        }
    }
    let identity = json!({"name": name, "version": version, "type": client_type, "url": url});
    {
        let mut clients = app.clients.lock().expect("clients lock");
        let client = clients.get_mut(&conn).ok_or_else(|| ApiError::bad_request("Connection closed"))?;
        if client.identity.is_some() {
            return Err(ApiError::bad_request(format!("Connection already identified: {conn}")));
        }
        client.identity = Some(identity.clone());
    }
    if client_type == "agent"
        && let Err(e) = crate::extensions::register_agent(&app, conn, &name, &identity).await
    {
        // As in Moonraker, the connection stays identified, but not as an agent.
        if let Some(c) = app.clients.lock().expect("clients lock").get_mut(&conn) {
            c.identity.as_mut().expect("identified above")["type"] = json!("");
        }
        return Err(e);
    }
    let transport = if app.client_peer(conn).unix { "Unix Socket" } else { "Websocket" };
    tracing::info!("{transport} {conn} Client Identified - Name: {name}, Version: {version}, Type: {client_type}");
    Ok(json!({"connection_id": conn}))
}

async fn websocket_id(_app: Arc<App>, req: Request) -> ApiResult {
    let conn = req.conn.ok_or_else(|| ApiError::bad_request("No connection associated with request"))?;
    Ok(json!({"websocket_id": conn}))
}
