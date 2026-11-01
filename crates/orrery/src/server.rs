//! The application: shared state, the event bus, websocket clients, JSON-RPC dispatch, and the
//! `server.*` endpoints that don't belong to a component.

use std::collections::HashMap;
use std::path::PathBuf;
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

/// Where orrery keeps its files, from `-d` and `-c`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub config_file: PathBuf,
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
    warnings: Mutex<Vec<String>>,
}

/// Moonraker's names for the components orrery implements, as `server.info` lists them.
const COMPONENTS: &[&str] = &[
    "application",
    "websockets",
    "klippy_connection",
    "database",
    "file_manager",
    "klippy_apis",
    "data_store",
    "job_state",
];

impl App {
    pub fn new(config: Config, paths: Paths) -> Result<Arc<Self>> {
        let (events, _) = broadcast::channel(1024);
        let server = config.section("server");
        let socket = server.get_str("klippy_uds_address", &paths.data.join("comms/klippy.sock").to_string_lossy());
        let database = Database::open(&config, &paths)?;
        let files = FileManager::new(&config, &paths)?;
        let data_store = DataStore::new(&config)?;
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
            warnings: Mutex::new(Vec::new()),
        });
        app.register_endpoints();
        crate::klippy::register_endpoints(&app);
        crate::database::register_endpoints(&app);
        crate::data_store::register_endpoints(&app);
        crate::files::register_endpoints(&app);
        Ok(app)
    }

    /// Starts the background tasks: the Klippy connection, notifications, and stores.
    pub fn start(self: &Arc<Self>) {
        tokio::spawn(crate::klippy::run(self.clone()));
        tokio::spawn(forward_notifications(self.clone()));
        crate::data_store::start(self);
        crate::files::start(self);
        crate::job_state::start(self);
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

    pub fn add_client(&self, tx: mpsc::UnboundedSender<String>) -> ConnId {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().expect("clients lock").insert(id, Client { tx, identity: None });
        id
    }

    pub fn remove_client(&self, id: ConnId) {
        self.clients.lock().expect("clients lock").remove(&id);
        self.klippy.remove_subscription(crate::klippy::Subscriber::Conn(id));
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
        // A message without a method is a client's reply to a server-initiated call; orrery
        // makes none yet.
        let method = obj.get("method")?;
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
        let req = Request {
            endpoint: endpoint.endpoint.clone(),
            kind,
            args: Args(args),
            transport: Transport::Websocket,
            conn,
        };
        let result = (endpoint.handler)(self.clone(), req).await;
        if id.is_null() {
            return None;
        }
        Some(match result {
            Ok(v) => json!({"jsonrpc": "2.0", "result": v, "id": id}),
            Err(e) => {
                tracing::info!("JSON-RPC Request Error - Requested Method: {method}, Code: {}, Message: {e}", e.status);
                rpc_error(e.jsonrpc_code(), &e.message, id)
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
    }
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
        "components": COMPONENTS,
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
    let mut clients = app.clients.lock().expect("clients lock");
    let client = clients.get_mut(&conn).ok_or_else(|| ApiError::bad_request("Connection closed"))?;
    if client.identity.is_some() {
        return Err(ApiError::bad_request(format!("Connection already identified: {conn}")));
    }
    tracing::info!("Websocket {conn} Client Identified - Name: {name}, Version: {version}, Type: {client_type}");
    client.identity = Some(json!({"name": name, "version": version, "type": client_type, "url": url}));
    Ok(json!({"connection_id": conn}))
}

async fn websocket_id(_app: Arc<App>, req: Request) -> ApiResult {
    let conn = req.conn.ok_or_else(|| ApiError::bad_request("No connection associated with request"))?;
    Ok(json!({"websocket_id": conn}))
}
