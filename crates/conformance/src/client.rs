//! Websocket JSON-RPC and HTTP clients for the server under test.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use orrery_proto::jsonrpc::{ErrorObject, Incoming, Request};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// What a call returned: `Ok(result)` or the error object.
pub type Outcome = Result<Value, ErrorObject>;

const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// A notification as received.
#[derive(Debug, Clone)]
pub struct Received {
    pub method: String,
    pub params: Value,
}

#[derive(Default)]
struct Inbox {
    pending: HashMap<u64, oneshot::Sender<Outcome>>,
    /// Raw text frames that weren't a response to a pending call (for `raw_ws` steps).
    raw: Vec<Value>,
    notifications: Vec<Received>,
}

/// A websocket connection to `/websocket`.
pub struct WsClient {
    out: mpsc::UnboundedSender<Message>,
    inbox: Arc<Mutex<Inbox>>,
    arrived: Arc<Notify>,
    next_id: Mutex<u64>,
}

impl WsClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let (stream, _) =
            tokio_tungstenite::connect_async(url).await.with_context(|| format!("connecting to {url}"))?;
        let (mut sink, mut source) = stream.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let arrived = Arc::new(Notify::new());
        let (inbox_r, arrived_r) = (inbox.clone(), arrived.clone());
        tokio::spawn(async move {
            while let Some(Ok(msg)) = source.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                let mut inbox = inbox_r.lock().unwrap();
                match Incoming::from_value(value.clone()) {
                    Some(Incoming::Notification { method, params }) => {
                        inbox.notifications.push(Received { method, params });
                    }
                    Some(Incoming::Response { id, outcome }) => {
                        match id.as_u64().and_then(|id| inbox.pending.remove(&id)) {
                            Some(tx) => {
                                let _ = tx.send(outcome);
                            }
                            None => inbox.raw.push(value),
                        }
                    }
                    None => inbox.raw.push(value),
                }
                drop(inbox);
                arrived_r.notify_waiters();
            }
        });
        Ok(Self { out, inbox, arrived, next_id: Mutex::new(0) })
    }

    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Outcome> {
        let id = {
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            *next
        };
        let (tx, rx) = oneshot::channel();
        self.inbox.lock().unwrap().pending.insert(id, tx);
        let req = Request::new(method, params, Some(json!(id)));
        self.out.send(Message::text(serde_json::to_string(&req)?)).context("websocket closed")?;
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(_)) => bail!("websocket closed before {method} returned"),
            Err(_) => bail!("{method}: no response within {CALL_TIMEOUT:?}"),
        }
    }

    /// Sends a raw text frame and returns the next message that isn't a notification or a reply
    /// to another call.
    pub async fn raw(&self, text: &str, timeout: Duration) -> Result<Value> {
        self.inbox.lock().unwrap().raw.clear();
        self.out.send(Message::text(text.to_owned())).context("websocket closed")?;
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.arrived.notified();
            if let Some(v) = self.inbox.lock().unwrap().raw.first().cloned() {
                return Ok(v);
            }
            if tokio::time::timeout_at(deadline.into(), notified).await.is_err() {
                bail!("no reply to raw frame within {timeout:?}");
            }
        }
    }

    /// Buffered notifications, oldest first, without removing them.
    pub fn notifications(&self) -> Vec<Received> {
        self.inbox.lock().unwrap().notifications.clone()
    }

    /// Waits until a notification satisfies `pred`, and removes it and everything before it, so
    /// the next wait only sees later notifications.
    pub async fn wait_notification(&self, timeout: Duration, pred: impl Fn(&Received) -> bool) -> Option<Received> {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.arrived.notified();
            {
                let mut inbox = self.inbox.lock().unwrap();
                if let Some(pos) = inbox.notifications.iter().position(&pred) {
                    let found = inbox.notifications[pos].clone();
                    inbox.notifications.drain(..=pos);
                    return Some(found);
                }
            }
            if tokio::time::timeout_at(deadline.into(), notified).await.is_err() {
                return None;
            }
        }
    }

    pub fn drain(&self) {
        self.inbox.lock().unwrap().notifications.clear();
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub verb: String,
    pub path: Option<String>,
}

/// Sends JSON-RPC-style calls as HTTP requests, using the route table dumped from the reference.
pub struct HttpClient {
    base: String,
    routes: HashMap<String, Route>,
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new(base: String, routes: HashMap<String, Route>) -> Self {
        Self { base, routes, client: reqwest::Client::new() }
    }

    /// Resolves a method to its HTTP verb and path. Methods Klippy provides aren't in the table;
    /// Moonraker serves them at `/printer/<path>` for both GET and POST.
    pub fn route(&self, method: &str) -> Result<(String, String)> {
        match self.routes.get(method) {
            Some(Route { verb, path: Some(path) }) => Ok((verb.clone(), path.clone())),
            Some(Route { path: None, .. }) => bail!("{method} is websocket-only"),
            None if method.starts_with("printer.") => Ok(("POST".into(), format!("/{}", method.replace('.', "/")))),
            None => bail!("{method} isn't in conformance/http_routes.json"),
        }
    }

    pub async fn call(&self, method: &str, params: Option<&Value>) -> Result<Outcome> {
        let (verb, path) = self.route(method)?;
        let params = params.cloned().unwrap_or_else(|| json!({}));
        let Value::Object(params) = params else { bail!("{method}: HTTP needs object params") };
        let url = format!("{}{path}", self.base);
        let req = match verb.as_str() {
            "GET" | "DELETE" => {
                let query = scalar_query(method, &params)?;
                let builder = if verb == "GET" { self.client.get(url) } else { self.client.delete(url) };
                builder.query(&query)
            }
            _ => self.client.post(url).json(&params),
        };
        self.send(req).await
    }

    /// Sends an explicit request (`http:` override in a case).
    pub async fn request(&self, verb: &str, path: &str, query: Option<&str>, body: Option<&Value>) -> Result<Outcome> {
        let mut url = format!("{}{path}", self.base);
        if let Some(q) = query {
            url.push('?');
            url.push_str(q);
        }
        let verb = reqwest::Method::from_bytes(verb.as_bytes()).context("bad HTTP verb")?;
        let mut req = self.client.request(verb, url);
        if let Some(body) = body {
            req = req.json(body);
        }
        self.send(req).await
    }

    /// Uploads a file the way frontends do: multipart form data with a `file` part.
    pub async fn upload(&self, name: &str, content: Vec<u8>, form: &Map<String, Value>) -> Result<Outcome> {
        let mut multipart = reqwest::multipart::Form::new();
        for (k, v) in form {
            let text = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            multipart = multipart.text(k.clone(), text);
        }
        let part =
            reqwest::multipart::Part::bytes(content).file_name(name.to_owned()).mime_str("application/octet-stream")?;
        multipart = multipart.part("file", part);
        let url = format!("{}/server/files/upload", self.base);
        self.send(self.client.post(url).multipart(multipart)).await
    }

    /// Downloads a file. Returns the status and body.
    /// Returns the status, the `Content-Type` header, and the body.
    pub async fn download(&self, path: &str) -> Result<(u16, String, Vec<u8>)> {
        let url = format!("{}/server/files/{path}", self.base);
        let resp = self.client.get(url).timeout(CALL_TIMEOUT).send().await?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        Ok((status, content_type, resp.bytes().await?.to_vec()))
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Outcome> {
        let resp = req.timeout(CALL_TIMEOUT).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.context("response isn't JSON")?;
        if status.is_success() {
            return Ok(Ok(body.get("result").cloned().unwrap_or(body)));
        }
        // Moonraker: {"error": {"code": <status>, "message": ..., "traceback": ...}}
        let err = body.get("error").and_then(Value::as_object).cloned().unwrap_or_default();
        Ok(Err(ErrorObject {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(status.as_u16() as i64),
            message: err.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
            data: None,
        }))
    }
}

/// Query args for GET and DELETE. Moonraker parses them from strings, so only scalars work; a
/// case with structured params must use `http:` to spell out the request.
fn scalar_query(method: &str, params: &Map<String, Value>) -> Result<Vec<(String, String)>> {
    params
        .iter()
        .map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
                Value::Null => String::new(),
                _ => bail!("{method}: param {k:?} isn't a scalar; give the step an `http:` request"),
            };
            Ok((k.clone(), s))
        })
        .collect()
}
