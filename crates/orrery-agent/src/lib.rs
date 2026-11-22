//! A client for writing orrery agents in Rust.
//!
//! An agent is a separate program that connects to orrery's unix socket and identifies as an
//! agent. It can answer calls that clients make to it, register endpoints and Klippy remote
//! methods, send events, add fields to print history, and call any of orrery's methods, such as
//! `printer.objects.subscribe`. See `docs/design/2026-11-15-plugin-interface.md`.
//!
//! ```no_run
//! use orrery_agent::{AgentBuilder, MethodError};
//! use serde_json::json;
//!
//! # async fn example() -> Result<(), orrery_agent::Error> {
//! let agent = AgentBuilder::new("hello")
//!     .method("greet", |params| async move {
//!         let name = params["name"].as_str().ok_or_else(|| MethodError::invalid_params("name is missing"))?;
//!         Ok(json!({"greeting": format!("Hello, {name}!")}))
//!     })
//!     .connect()
//!     .await?;
//! // Clients call it as `agent.hello.greet`, or `GET /agent/hello/greet`.
//! agent.register_endpoint("greet", &["GET"]).await?;
//! agent.closed().await;
//! # Ok(())
//! # }
//! ```
//!
//! [`run`] keeps an agent connected: it connects again after orrery restarts, and runs the
//! agent's setup each time.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const ETX: u8 = 0x03;

/// What can go wrong talking to orrery.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("can't connect to orrery at {path}: {source}")]
    Connect { path: PathBuf, source: std::io::Error },
    #[error("the connection to orrery closed")]
    Closed,
    /// orrery, or the method it ran, answered with an error.
    #[error("{method} failed: {message} (code {code})")]
    Rpc { method: String, code: i64, message: String, data: Option<Value> },
}

/// An error a method handler returns. Through `server.extensions.request`, the client gets it as
/// the `data` of Moonraker's 424 error. Through one of the agent's endpoints, `code` is the HTTP
/// status, such as 503; a code outside 400 to 599 becomes a 500.
#[derive(Debug, Clone)]
pub struct MethodError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl MethodError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    /// JSON-RPC's -32602.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl<E: std::fmt::Display> From<E> for MethodError {
    /// Any other error becomes JSON-RPC's -32000, with its text as the message.
    fn from(e: E) -> Self {
        Self::new(-32000, e.to_string())
    }
}

/// A message orrery pushed to the agent: a notification such as `notify_status_update`, or a
/// Klippy remote method call.
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// A field an agent adds to the print job being recorded (see [`Agent::history_field`]).
#[derive(Debug, Clone)]
pub struct HistoryField {
    pub name: String,
    pub value: Value,
    pub description: Option<String>,
    pub units: Option<String>,
}

type MethodFuture = Pin<Box<dyn Future<Output = Result<Value, MethodError>> + Send>>;
type Method = Arc<dyn Fn(Value) -> MethodFuture + Send + Sync>;

/// How to connect: the agent's name, its methods, and where orrery is.
#[derive(Clone)]
pub struct AgentBuilder {
    name: String,
    version: String,
    url: String,
    socket: Option<PathBuf>,
    methods: HashMap<String, Method>,
}

impl AgentBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), version: "0.0.0".into(), url: String::new(), socket: None, methods: HashMap::new() }
    }

    /// The version clients see in `server.extensions.list`.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// A link about the agent, shown in `server.extensions.list`.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// orrery's unix socket. The default is `$ORRERY_SOCKET`, then `$MOONRAKER_UDS_PATH`, then
    /// `~/printer_data/comms/moonraker.sock`.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    /// Answers calls to `name`: calls relayed with `server.extensions.request`, calls to the
    /// agent's endpoints (by the method name after the agent's prefix, such as `get_spool_id`),
    /// and Klippy remote method calls, whose results are dropped.
    pub fn method<F, Fut>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, MethodError>> + Send + 'static,
    {
        self.methods.insert(name.into(), Arc::new(move |params| Box::pin(f(params))));
        self
    }

    fn socket_path(&self) -> PathBuf {
        if let Some(path) = &self.socket {
            return path.clone();
        }
        for var in ["ORRERY_SOCKET", "MOONRAKER_UDS_PATH"] {
            if let Some(path) = std::env::var_os(var).filter(|p| !p.is_empty()) {
                return path.into();
            }
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join("printer_data/comms/moonraker.sock")
    }

    /// Connects and identifies as an agent.
    pub async fn connect(self) -> Result<Agent, Error> {
        let path = self.socket_path();
        let stream =
            UnixStream::connect(&path).await.map_err(|source| Error::Connect { path: path.clone(), source })?;
        let agent = Agent::start(stream, self.methods.clone());
        let identity = json!({
            "client_name": self.name,
            "version": self.version,
            "type": "agent",
            "url": self.url,
        });
        agent.call("server.connection.identify", identity).await?;
        // orrery logs "Agent X connected".
        tracing::debug!("Connected to orrery at {} as agent {}", path.display(), self.name);
        Ok(agent)
    }
}

/// A connected agent. Clones share the connection.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

struct Inner {
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>,
    next_id: AtomicU64,
    notifications: broadcast::Sender<Notification>,
    closed: watch::Receiver<bool>,
    /// Tells the writer to close the connection.
    shutdown: watch::Sender<bool>,
}

impl Agent {
    fn start(stream: UnixStream, methods: HashMap<String, Method>) -> Self {
        let (reader, mut writer) = stream.into_split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (closed_tx, closed) = watch::channel(false);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            notifications: broadcast::channel(256).0,
            closed,
            shutdown,
        });
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    text = out_rx.recv() => {
                        let Some(text) = text else { break };
                        let mut data = text.into_bytes();
                        data.push(ETX);
                        if writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    // The guard `wait_for` returns isn't `Send`, so it's dropped inside.
                    _ = async { drop(shutdown_rx.wait_for(|s| *s).await) } => break,
                }
            }
            // orrery sees the end of the stream and closes its side, which ends the reader.
            let _ = writer.shutdown().await;
        });
        let reader_inner = inner.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(ETX, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if buf.pop() != Some(ETX) {
                    break;
                }
                match serde_json::from_slice::<Value>(&buf) {
                    Ok(msg) => reader_inner.receive(msg, &methods),
                    Err(e) => tracing::warn!("orrery sent a message that isn't JSON: {e}"),
                }
            }
            // Marked closed first, so a call that starts now fails at once; then the calls still
            // waiting fail with `Closed`.
            let _ = closed_tx.send(true);
            reader_inner.pending.lock().expect("pending lock").clear();
        });
        Self { inner }
    }

    /// Calls one of orrery's methods, such as `printer.objects.query`, and returns its result.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().expect("pending lock").insert(id, tx);
        if *self.inner.closed.borrow() {
            self.inner.pending.lock().expect("pending lock").remove(&id);
            return Err(Error::Closed);
        }
        let mut msg = json!({"jsonrpc": "2.0", "method": method, "id": id});
        if !params.is_null() {
            msg["params"] = params;
        }
        self.inner.out.send(msg.to_string()).map_err(|_| Error::Closed)?;
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(Error::Rpc {
                method: method.to_owned(),
                code: error["code"].as_i64().unwrap_or_default(),
                message: error["message"].as_str().unwrap_or_default().to_owned(),
                data: error.get("data").cloned(),
            }),
            Err(_) => Err(Error::Closed),
        }
    }

    /// Adds an endpoint at `/agent/<name>/<path>`, served for the HTTP `request_methods`
    /// ("GET", "POST", "DELETE"). Calls reach the method of the same JSON-RPC name: `path`
    /// itself for one request method, or `get_<path>` and so on for several.
    pub async fn register_endpoint(&self, path: &str, request_methods: &[&str]) -> Result<(), Error> {
        self.call("orrery.agent.register_endpoint", json!({"path": path, "request_methods": request_methods}))
            .await
            .map(drop)
    }

    /// Lets Klippy macros call `name` with `action_call_remote_method`. The call reaches the
    /// method of the same name.
    pub async fn register_remote_method(&self, name: &str) -> Result<(), Error> {
        self.call("connection.register_remote_method", json!({"method_name": name})).await.map(drop)
    }

    /// Sends an event to the other clients, as `notify_agent_event` or under the name
    /// `orrery.toml` maps it to.
    pub async fn send_event(&self, event: &str, data: Option<Value>) -> Result<(), Error> {
        let mut params = json!({"event": event});
        if let Some(data) = data {
            params["data"] = data;
        }
        self.call("connection.send_event", params).await.map(drop)
    }

    /// Sets a field on the print job being recorded, saved in the job's `auxiliary_data`. The
    /// agent's `[agent.<name>]` table must list the field in `history_fields`.
    pub async fn history_field(&self, field: HistoryField) -> Result<(), Error> {
        let params = json!({
            "name": field.name,
            "value": field.value,
            "description": field.description,
            "units": field.units,
        });
        self.call("orrery.agent.history_field", params).await.map(drop)
    }

    /// The agent's own config section: by default the one named after the agent, such as
    /// `[spoolman]`. Values from `orrery.toml` keep their types; values from `moonraker.conf`
    /// are text. `None` if neither file has the section.
    pub async fn config(&self, section: Option<&str>) -> Result<Option<Map<String, Value>>, Error> {
        let params = section.map_or(Value::Null, |s| json!({"section": s}));
        let reply = self.call("orrery.agent.config", params).await?;
        Ok(match (reply["exists"].as_bool(), reply.get("options")) {
            (Some(true), Some(Value::Object(options))) => Some(options.clone()),
            _ => None,
        })
    }

    /// The names of the agent's named sections: `crowsnest` for `[update_manager crowsnest]`,
    /// when the section is `update_manager`. `None` asks about the section named after the agent.
    pub async fn config_names(&self, section: Option<&str>) -> Result<Vec<String>, Error> {
        let params = section.map_or(Value::Null, |s| json!({"section": s}));
        let reply = self.call("orrery.agent.config", params).await?;
        Ok(reply["named"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(str::to_owned)).collect())
    }

    /// Replaces the agent's own warnings in `server.info`, such as a config entry it can't use.
    /// An empty list clears them, and so does disconnecting.
    pub async fn set_warnings(&self, warnings: &[String]) -> Result<(), Error> {
        self.call("orrery.agent.set_warnings", json!({"warnings": warnings})).await.map(drop)
    }

    /// Notifications and Klippy remote method calls orrery pushes, from now on.
    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.inner.notifications.subscribe()
    }

    /// Closes the connection, and waits until orrery has closed its side.
    pub async fn close(&self) {
        let _ = self.inner.shutdown.send(true);
        self.closed().await;
    }

    /// Waits until the connection closes.
    pub async fn closed(&self) {
        let mut closed = self.inner.closed.clone();
        let _ = closed.wait_for(|c| *c).await;
    }
}

impl Inner {
    fn receive(self: &Arc<Self>, msg: Value, methods: &HashMap<String, Method>) {
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A reply to one of the agent's calls.
            let Some(id) = msg.get("id").and_then(Value::as_u64) else { return };
            let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) else { return };
            let reply = match msg.get("error") {
                Some(error) => Err(error.clone()),
                None => Ok(msg.get("result").cloned().unwrap_or_default()),
            };
            let _ = tx.send(reply);
            return;
        };
        let params = msg.get("params").cloned().unwrap_or_default();
        let handler = methods.get(method).cloned();
        let Some(id) = msg.get("id").cloned() else {
            // A notification, or a Klippy remote method call.
            let _ = self.notifications.send(Notification { method: method.to_owned(), params: params.clone() });
            if let Some(handler) = handler {
                let method = method.to_owned();
                tokio::spawn(async move {
                    if let Err(e) = handler(params).await {
                        tracing::warn!("Remote method {method} failed: {}", e.message);
                    }
                });
            }
            return;
        };
        // A call from a client, which waits for the reply.
        let inner = self.clone();
        let method = method.to_owned();
        tokio::spawn(async move {
            let outcome = match handler {
                Some(handler) => handler(params).await,
                None => Err(MethodError::new(-32601, format!("Method not found: {method}"))),
            };
            let reply = match outcome {
                Ok(result) => json!({"jsonrpc": "2.0", "result": result, "id": id}),
                Err(e) => {
                    let mut error = json!({"code": e.code, "message": e.message});
                    if let Some(data) = e.data {
                        error["data"] = data;
                    }
                    json!({"jsonrpc": "2.0", "error": error, "id": id})
                }
            };
            let _ = inner.out.send(reply.to_string());
        });
    }
}

/// Keeps an agent connected. It connects, runs `setup` (register endpoints and remote methods,
/// subscribe to printer objects, and so on), and waits until the connection closes, then does
/// it all again. Failed connections are retried with a growing delay, up to 30 seconds.
pub async fn run<F, Fut>(builder: AgentBuilder, setup: F) -> !
where
    F: Fn(Agent) -> Fut,
    Fut: Future<Output = Result<(), Error>>,
{
    let mut delay = Duration::from_secs(1);
    loop {
        match builder.clone().connect().await {
            Ok(agent) => match setup(agent.clone()).await {
                Ok(()) => {
                    delay = Duration::from_secs(1);
                    agent.closed().await;
                    tracing::info!("The connection to orrery closed; connecting again");
                }
                Err(e) => {
                    tracing::warn!("Agent setup failed: {e}");
                    // Otherwise orrery still has the agent's name when it connects again.
                    agent.close().await;
                }
            },
            Err(e) => tracing::warn!("{e}"),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// The socket path an agent connects to by default, for logging and error messages.
pub fn default_socket() -> PathBuf {
    AgentBuilder::new("").socket_path()
}

/// Whether stdout goes to the systemd journal, as when a systemd unit runs the agent. The
/// journal records when each line came, so the agent's log can leave out the time. systemd
/// sets `JOURNAL_STREAM` to the stream's device and inode; a process with other output can
/// inherit the variable, so it's compared with stdout.
pub fn stdout_is_journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some((dev, ino)) = std::env::var("JOURNAL_STREAM").ok().and_then(|s| {
        let (dev, ino) = s.split_once(':')?;
        Some((dev.parse::<u64>().ok()?, ino.parse::<u64>().ok()?))
    }) else {
        return false;
    };
    std::fs::metadata("/proc/self/fd/1").is_ok_and(|m| m.dev() == dev && m.ino() == ino)
}

/// Lines for the journal, such as `<4>WARN orrery_agent: …`, as orrery writes them: journald
/// takes the `<4>` as the line's priority.
struct JournalFormat;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for JournalFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let priority = match *meta.level() {
            tracing::Level::ERROR => 3,
            tracing::Level::WARN => 4,
            tracing::Level::INFO => 6,
            _ => 7,
        };
        write!(writer, "<{priority}>{} {}: ", meta.level(), meta.target())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Sets up logging for an agent: `RUST_LOG`, or `info` by default. Lines for the journal carry
/// their priority, and lines for the journal or orrery's log leave out the time, which both add.
pub fn init_logging() {
    let log = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()));
    if stdout_is_journal() {
        log.event_format(JournalFormat).init();
    } else if std::env::var_os("ORRERY_AGENT").is_some() {
        log.without_time().with_ansi(false).init();
    } else {
        log.init();
    }
}
