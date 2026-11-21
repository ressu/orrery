//! The endpoint registry shared by HTTP and JSON-RPC, after Moonraker's `APIDefinition`.
//!
//! An endpoint is an HTTP path with one or more request types. Its JSON-RPC method names come
//! from the path: `/server/info` (GET) is `server.info`, and `/server/database/item` (GET, POST,
//! DELETE) is `server.database.get_item`, `post_item`, and `delete_item`. Endpoints that Klippy
//! provides ("remote" endpoints) are served at `/printer/<endpoint>` for GET and POST, and as
//! `printer.<endpoint with dots>` over JSON-RPC.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::server::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestType {
    Get,
    Post,
    Delete,
}

impl RequestType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Delete => "delete",
        }
    }

    pub fn from_http(method: &str) -> Option<Self> {
        match method {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "DELETE" => Some(Self::Delete),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Http,
    Websocket,
}

/// An error with an HTTP status. Over JSON-RPC, 404 becomes -32601 and 401 becomes -32602, as in
/// Moonraker; other statuses are sent as the code unchanged.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Extra detail sent as the JSON-RPC error's `data`: an agent's own error.
    pub data: Option<Value>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Moonraker's default `ServerError` status.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    pub fn jsonrpc_code(&self) -> i64 {
        match self.status {
            404 => -32601,
            401 => -32602,
            s => s as i64,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::new(500, format!("{e:#}"))
    }
}

pub type ApiResult = Result<Value, ApiError>;

/// Identifies a websocket connection. Moonraker uses Python object ids, which are large
/// numbers; orrery hands out its own.
pub type ConnId = u64;

pub struct Request {
    /// The endpoint as registered: an HTTP path, or a Klippy endpoint like `objects/query`.
    pub endpoint: String,
    pub kind: RequestType,
    pub args: Args,
    pub transport: Transport,
    /// The websocket the request came over, or the one an HTTP request named.
    pub conn: Option<ConnId>,
    /// What the client may do. The request has passed its endpoint's check already.
    pub access: crate::auth::Access,
}

/// Request arguments with Moonraker's conversions (`WebRequest.get_str`, `get_int`, ...).
/// HTTP query values arrive as strings and are converted on read.
#[derive(Debug, Clone, Default)]
pub struct Args(pub Map<String, Value>);

impl Args {
    fn missing(key: &str) -> ApiError {
        ApiError::bad_request(format!("No data for argument: {key}"))
    }

    fn convert_error(key: &str, kind: &str, v: &Value) -> ApiError {
        let shown = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        ApiError::bad_request(format!("Unable to convert argument [{key}] to {kind}: value received: {shown}"))
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn require(&self, key: &str) -> Result<&Value, ApiError> {
        self.0.get(key).ok_or_else(|| Self::missing(key))
    }

    pub fn str(&self, key: &str) -> Result<String, ApiError> {
        Ok(match self.require(key)? {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            Value::Bool(b) => if *b { "True" } else { "False" }.into(),
            other => other.to_string(),
        })
    }

    pub fn opt_str(&self, key: &str) -> Result<Option<String>, ApiError> {
        if self.0.contains_key(key) { self.str(key).map(Some) } else { Ok(None) }
    }

    pub fn int(&self, key: &str) -> Result<i64, ApiError> {
        let v = self.require(key)?;
        match v {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
        .ok_or_else(|| Self::convert_error(key, "<class 'int'>", v))
    }

    pub fn opt_int(&self, key: &str) -> Result<Option<i64>, ApiError> {
        if self.0.contains_key(key) { self.int(key).map(Some) } else { Ok(None) }
    }

    pub fn float(&self, key: &str) -> Result<f64, ApiError> {
        let v = self.require(key)?;
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
        .ok_or_else(|| Self::convert_error(key, "<class 'float'>", v))
    }

    pub fn opt_float(&self, key: &str) -> Result<Option<f64>, ApiError> {
        if self.0.contains_key(key) { self.float(key).map(Some) } else { Ok(None) }
    }

    /// A list of strings, as `WebRequest.get_list`: a list, or a comma-separated string.
    pub fn list(&self, key: &str) -> Result<Vec<String>, ApiError> {
        match self.require(key)? {
            Value::String(s) => Ok(s.split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect()),
            Value::Array(items) => {
                items.iter().map(|v| v.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>().ok_or_else(|| {
                    ApiError::bad_request(format!(
                        "Invalid list format for argument '{key}', expected all values to be of type str."
                    ))
                })
            }
            other => Err(ApiError::bad_request(format!(
                "Invalid value received for argument '{key}'.  Expected List type, received {}",
                python_type(other)
            ))),
        }
    }

    pub fn bool(&self, key: &str) -> Result<bool, ApiError> {
        let v = self.require(key)?;
        match v {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
        .ok_or_else(|| Self::convert_error(key, "<class 'bool'>", v))
    }

    pub fn opt_bool(&self, key: &str) -> Result<Option<bool>, ApiError> {
        if self.0.contains_key(key) { self.bool(key).map(Some) } else { Ok(None) }
    }
}

/// Python's name for a JSON value's type, for error messages that match Moonraker's.
pub fn python_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

pub type HandlerFuture = Pin<Box<dyn Future<Output = ApiResult> + Send>>;
pub type Handler = Arc<dyn Fn(Arc<App>, Request) -> HandlerFuture + Send + Sync>;

/// Wraps an async function as a [`Handler`].
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Arc<App>, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ApiResult> + Send + 'static,
{
    Arc::new(move |app, req| Box::pin(f(app, req)))
}

#[derive(Clone)]
pub struct Endpoint {
    pub endpoint: String,
    pub types: Vec<RequestType>,
    pub http: bool,
    /// HTTP query args become `objects` (Klippy's `objects/*` endpoints).
    pub object_parser: bool,
    pub handler: Handler,
}

#[derive(Default)]
pub struct Registry {
    by_path: HashMap<String, Endpoint>,
    /// JSON-RPC method -> (HTTP path, request type).
    methods: HashMap<String, (String, RequestType)>,
    /// Method prefixes `orrery.toml` maps to agents. They're checked before everything else.
    aliases: Vec<Alias>,
}

/// A method prefix mapped to an agent's prefix, such as `server.spoolman` to
/// `agent.spoolman`, with the matching HTTP paths.
#[derive(Debug, Clone)]
pub struct Alias {
    pub public_method: String,
    pub agent_method: String,
    pub public_path: String,
    pub agent_path: String,
    /// The agent the prefix belongs to.
    pub agent: String,
}

impl Alias {
    pub fn new(public: &str, target: &str, agent: &str) -> Self {
        Self {
            public_method: public.to_owned(),
            agent_method: target.to_owned(),
            public_path: format!("/{}", public.replace('.', "/")),
            agent_path: format!("/{}", target.replace('.', "/")),
            agent: agent.to_owned(),
        }
    }
}

/// `name` with `prefix` replaced by `with`, when `name` is `prefix` or starts with `prefix` and
/// then `sep`.
fn swap_prefix(name: &str, prefix: &str, with: &str, sep: char) -> Option<String> {
    let rest = name.strip_prefix(prefix)?;
    (rest.is_empty() || rest.starts_with(sep)).then(|| format!("{with}{rest}"))
}

/// What a mapped name answers when the agent hasn't registered it: "not connected" while the
/// agent is away, "not found" otherwise.
fn agent_stub(path: &str, agent: &str) -> Endpoint {
    let agent = agent.to_owned();
    Endpoint {
        endpoint: path.to_owned(),
        types: vec![RequestType::Get, RequestType::Post, RequestType::Delete],
        http: true,
        object_parser: false,
        handler: handler(move |app: Arc<App>, _req| {
            let agent = agent.clone();
            async move {
                if crate::extensions::is_connected(&app, &agent) {
                    Err(ApiError::not_found("Not Found"))
                } else {
                    Err(ApiError::bad_request(format!("Agent {agent} not connected")))
                }
            }
        }),
    }
}

/// Which transports an endpoint is served on.
#[derive(Debug, Clone, Copy)]
pub enum Transports {
    All,
    WebsocketOnly,
}

impl Registry {
    pub fn register(&mut self, path: &str, types: &[RequestType], transports: Transports, handler: Handler) {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (last, prefix) = parts.split_last().expect("endpoint paths aren't empty");
        for &t in types {
            let method = if types.len() > 1 {
                let mut p: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
                p.push(format!("{}_{last}", t.as_str()));
                p.join(".")
            } else {
                parts.join(".")
            };
            self.methods.insert(method, (path.to_owned(), t));
        }
        self.by_path.insert(
            path.to_owned(),
            Endpoint {
                endpoint: path.to_owned(),
                types: types.to_vec(),
                http: matches!(transports, Transports::All),
                object_parser: false,
                handler,
            },
        );
    }

    /// Registers an endpoint Klippy provides. Returns false if it was already registered.
    pub fn register_remote(&mut self, endpoint: &str, handler: Handler) -> bool {
        let http_path = format!("/printer/{}", endpoint.trim_matches('/'));
        if self.by_path.contains_key(&http_path) {
            return false;
        }
        let method = http_path[1..].replace('/', ".");
        // Remote endpoints take any request type; GET is recorded for JSON-RPC.
        self.methods.insert(method, (http_path.clone(), RequestType::Get));
        self.by_path.insert(
            http_path,
            Endpoint {
                endpoint: endpoint.to_owned(),
                types: vec![RequestType::Get, RequestType::Post],
                http: true,
                object_parser: endpoint.starts_with("objects/"),
                handler,
            },
        );
        true
    }

    pub fn by_method(&self, method: &str) -> Option<(Endpoint, RequestType)> {
        for alias in &self.aliases {
            if let Some(target) = swap_prefix(method, &alias.public_method, &alias.agent_method, '.') {
                return Some(
                    self.direct_method(&target)
                        .unwrap_or_else(|| (agent_stub(&alias.agent_path, &alias.agent), RequestType::Post)),
                );
            }
        }
        self.direct_method(method)
    }

    fn direct_method(&self, method: &str) -> Option<(Endpoint, RequestType)> {
        let (path, t) = self.methods.get(method)?;
        Some((self.by_path.get(path)?.clone(), *t))
    }

    pub fn by_path(&self, path: &str) -> Option<Endpoint> {
        for alias in &self.aliases {
            if let Some(target) = swap_prefix(path, &alias.public_path, &alias.agent_path, '/') {
                return Some(self.by_path.get(&target).cloned().unwrap_or_else(|| agent_stub(&target, &alias.agent)));
            }
        }
        self.by_path.get(path).cloned()
    }

    /// The JSON-RPC method name of an endpoint for one request type.
    pub fn method_name(&self, path: &str, kind: RequestType) -> String {
        self.methods
            .iter()
            .find(|(_, (p, t))| p == path && *t == kind)
            .map(|(m, _)| m.clone())
            .unwrap_or_else(|| path.trim_start_matches('/').replace('/', "."))
    }

    /// True if any registered method is `prefix` or starts with `prefix.`.
    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.methods.keys().any(|m| swap_prefix(m, prefix, "", '.').is_some())
    }

    pub fn set_aliases(&mut self, aliases: Vec<Alias>) {
        self.aliases = aliases;
    }

    /// Removes every endpoint whose path starts with `prefix`.
    pub fn unregister_prefix(&mut self, prefix: &str) {
        self.by_path.retain(|p, _| !p.starts_with(prefix));
        self.methods.retain(|_, (p, _)| !p.starts_with(prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn noop() -> Handler {
        handler(|_, _| async { Ok(json!("ok")) })
    }

    #[test]
    fn method_names() {
        let mut r = Registry::default();
        r.register("/server/info", &[RequestType::Get], Transports::All, noop());
        r.register(
            "/server/database/item",
            &[RequestType::Get, RequestType::Post, RequestType::Delete],
            Transports::All,
            noop(),
        );
        r.register_remote("objects/query", noop());
        assert!(r.by_method("server.info").is_some());
        assert_eq!(r.by_method("server.database.post_item").unwrap().1, RequestType::Post);
        assert!(r.by_method("server.database.item").is_none());
        let (ep, _) = r.by_method("printer.objects.query").unwrap();
        assert_eq!(ep.endpoint, "objects/query");
        assert!(ep.object_parser);
        assert!(r.by_path("/printer/objects/query").is_some());
    }

    #[test]
    fn aliases() {
        let mut r = Registry::default();
        r.register("/server/info", &[RequestType::Get], Transports::All, noop());
        r.register("/agent/spoolman/spool_id", &[RequestType::Get, RequestType::Post], Transports::All, noop());
        r.set_aliases(vec![Alias::new("server.spoolman", "agent.spoolman", "spoolman")]);
        let (ep, t) = r.by_method("server.spoolman.post_spool_id").unwrap();
        assert_eq!((ep.endpoint.as_str(), t), ("/agent/spoolman/spool_id", RequestType::Post));
        assert_eq!(r.by_path("/server/spoolman/spool_id").unwrap().endpoint, "/agent/spoolman/spool_id");
        assert_eq!(r.method_name("/agent/spoolman/spool_id", RequestType::Get), "agent.spoolman.get_spool_id");
        // A mapped name the agent hasn't registered still answers, with the stub.
        assert_eq!(r.by_method("server.spoolman.proxy").unwrap().0.endpoint, "/agent/spoolman");
        assert_eq!(r.by_path("/server/spoolman/proxy").unwrap().endpoint, "/agent/spoolman/proxy");
        // Only whole prefixes match.
        assert!(r.by_method("server.spoolmanx.proxy").is_none());
        assert!(r.has_prefix("server"));
        assert!(!r.has_prefix("serv"));
        r.unregister_prefix("/agent/spoolman/");
        assert!(r.direct_method("agent.spoolman.get_spool_id").is_none());
    }

    #[test]
    fn arg_conversions() {
        let a = Args(json!({"n": "12", "b": "TRUE", "s": 3}).as_object().unwrap().clone());
        assert_eq!(a.int("n").unwrap(), 12);
        assert!(a.bool("b").unwrap());
        assert_eq!(a.str("s").unwrap(), "3");
        assert_eq!(a.str("x").unwrap_err().message, "No data for argument: x");
        let l = Args(json!({"s": "a.gcode, b.gcode,", "l": ["c"], "bad": [1]}).as_object().unwrap().clone());
        assert_eq!(l.list("s").unwrap(), ["a.gcode", "b.gcode"]);
        assert_eq!(l.list("l").unwrap(), ["c"]);
        assert!(l.list("bad").is_err());
    }
}
