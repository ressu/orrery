//! HTTP and websocket transport. Every registered endpoint is served at its HTTP path, and the
//! websocket at `/websocket` speaks JSON-RPC 2.0.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::api::{ApiError, Args, Request, RequestType, Transport};
use crate::server::App;

pub fn router(app: Arc<App>) -> Router {
    let upload_limit = app.files.max_upload_size;
    Router::new()
        .route("/websocket", get(websocket))
        .route("/server/files/upload", post(crate::files::http::upload).layer(DefaultBodyLimit::max(upload_limit)))
        .fallback(http_request)
        .layer(axum::middleware::from_fn_with_state(app.clone(), cors))
        .with_state(app)
}

/// Adds CORS headers for allowed origins and answers their preflight requests, as Moonraker's
/// `_set_cors_headers` does.
async fn cors(State(app): State<Arc<App>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).cloned().filter(|o| o.to_str().is_ok_and(|o| app.cors.allows(o)));
    let Some(origin) = origin else { return next.run(req).await };
    let preflight = req.method() == Method::OPTIONS;
    let private_network =
        req.headers().get("Access-Control-Request-Private-Network").is_some_and(|v| v.as_bytes() == b"true");
    let mut resp = if preflight { StatusCode::NO_CONTENT.into_response() } else { next.run(req).await };
    let headers = resp.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    if preflight {
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, PUT, DELETE, OPTIONS".parse().expect("valid"));
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "Origin, Accept, Content-Type, X-Requested-With, X-CRSF-Token, Authorization, X-Access-Token, X-Api-Key"
                .parse()
                .expect("valid"),
        );
        if private_network {
            headers.insert("Access-Control-Allow-Private-Network", "true".parse().expect("valid"));
        }
    }
    resp
}

async fn websocket(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let from = describe_peer(peer, &headers);
    ws.on_upgrade(move |socket| serve_socket(app, socket, from))
}

/// Where a connection comes from, for the log: the client's address, the proxy's address when a
/// reverse proxy such as nginx or caddy forwarded it, and the client's `User-Agent`. This helps
/// tell apart clients that never call `server.connection.identify`.
fn describe_peer(peer: SocketAddr, headers: &HeaderMap) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let forwarded = header("x-forwarded-for").and_then(|v| v.split(',').next()).map(str::trim).or(header("x-real-ip"));
    let mut from = match forwarded {
        Some(client) => format!("{client} (via {peer})"),
        None => peer.to_string(),
    };
    if let Some(agent) = header("user-agent") {
        from.push_str(&format!(", User Agent: {agent}"));
    }
    from
}

async fn serve_socket(app: Arc<App>, socket: WebSocket, from: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = app.add_client(tx.clone(), crate::server::Peer::default());
    let opened = Instant::now();
    tracing::info!("Websocket Opened: ID: {conn}, From: {from}");
    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Close(_) => break,
            _ => continue,
        };
        // Requests run side by side, as in Moonraker: a slow one doesn't hold up the rest.
        let app = app.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(reply) = app.dispatch_jsonrpc(&text, Some(conn)).await {
                let _ = tx.send(reply.to_string());
            }
        });
    }
    let name = app.client_name(conn);
    app.remove_client(conn);
    writer.abort();
    tracing::info!("Websocket Closed: ID: {conn}, {}", crate::server::describe_close(name, opened));
}

pub fn error_response(status: u16, message: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({"error": {"code": status, "message": message}});
    (code, [(header::CONTENT_TYPE, "application/json; charset=UTF-8")], body.to_string()).into_response()
}

async fn http_request(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let found = app.registry.read().expect("registry lock").by_path(uri.path());
    let Some(endpoint) = found.filter(|e| e.http) else {
        if let Some(resp) = crate::files::http::file_request(&app, &method, uri.path(), &headers).await {
            return resp;
        }
        return error_response(404, "Not Found");
    };
    let Some(kind) = RequestType::from_http(method.as_str()).filter(|k| endpoint.types.contains(k)) else {
        return error_response(405, "Method Not Allowed");
    };
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut args = if endpoint.object_parser { object_args(&pairs) } else { default_args(&pairs) };
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim().starts_with("application/json"));
    if is_json && let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&body) {
        args.extend(body);
    }
    let req =
        Request { endpoint: endpoint.endpoint.clone(), kind, args: Args(args), transport: Transport::Http, conn: None };
    match (endpoint.handler)(app, req).await {
        Ok(result) => {
            let body = json!({"result": result});
            ([(header::CONTENT_TYPE, "application/json; charset=UTF-8")], body.to_string()).into_response()
        }
        Err(ApiError { status, message, .. }) => error_response(status, &message),
    }
}

/// Query args as strings, with Moonraker's `name:type` hints (`int`, `float`, `bool`, `json`).
fn default_args(pairs: &[(String, String)]) -> Map<String, Value> {
    let mut args = Map::new();
    for (key, value) in pairs {
        // As in Moonraker, a hint is always stripped from the name, and a value that doesn't
        // convert (or has an unknown hint) stays a string.
        let Some((name, hint)) = key.rsplit_once(':') else {
            args.insert(key.clone(), json!(value));
            continue;
        };
        let converted = match hint {
            "int" => value.parse::<i64>().map(Value::from).ok(),
            "float" => value.parse::<f64>().ok().map(Value::from),
            "bool" => Some(Value::Bool(value.eq_ignore_ascii_case("true"))),
            "json" => serde_json::from_str(value).ok(),
            _ => None,
        };
        args.insert(name.to_owned(), converted.unwrap_or_else(|| json!(value)));
    }
    args
}

/// Query args for Klippy's `objects/*` endpoints: `print_stats=state,filename&webhooks` becomes
/// `{"objects": {"print_stats": ["state", "filename"], "webhooks": null}}`.
fn object_args(pairs: &[(String, String)]) -> Map<String, Value> {
    let objects: Map<String, Value> = pairs
        .iter()
        .map(|(k, v)| {
            let fields = if v.is_empty() { Value::Null } else { json!(v.split(',').collect::<Vec<_>>()) };
            (k.clone(), fields)
        })
        .collect();
    let mut args = Map::new();
    args.insert("objects".into(), Value::Object(objects));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_hints() {
        let pairs = vec![
            ("a:int".to_owned(), "5".to_owned()),
            ("b".to_owned(), "x".to_owned()),
            ("c:json".to_owned(), "[1]".to_owned()),
        ];
        let args = default_args(&pairs);
        assert_eq!(args["a"], json!(5));
        assert_eq!(args["b"], json!("x"));
        assert_eq!(args["c"], json!([1]));
    }

    #[test]
    fn objects() {
        let pairs =
            vec![("print_stats".to_owned(), "state,filename".to_owned()), ("webhooks".to_owned(), String::new())];
        assert_eq!(object_args(&pairs)["objects"], json!({"print_stats": ["state", "filename"], "webhooks": null}));
    }

    #[test]
    fn peers() {
        let peer: SocketAddr = "127.0.0.1:59894".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(describe_peer(peer, &headers), "127.0.0.1:59894");
        headers.insert("x-forwarded-for", "192.168.1.199, 10.0.0.1".parse().unwrap());
        headers.insert("user-agent", "HomeAssistant/2026.11 aiohttp/3.12".parse().unwrap());
        assert_eq!(
            describe_peer(peer, &headers),
            "192.168.1.199 (via 127.0.0.1:59894), User Agent: HomeAssistant/2026.11 aiohttp/3.12"
        );
    }
}
