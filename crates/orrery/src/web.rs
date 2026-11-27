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
    let mut router = Router::new()
        .route("/websocket", get(websocket))
        .route("/server/files/upload", post(crate::files::http::upload).layer(DefaultBodyLimit::max(upload_limit)));
    if app.octoprint.is_some() {
        router = router.route(
            "/api/files/local",
            post(crate::files::http::octoprint_upload).layer(DefaultBodyLimit::max(upload_limit)),
        );
    }
    router.fallback(http_request).layer(axum::middleware::from_fn_with_state(app.clone(), cors)).with_state(app)
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
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let from = describe_peer(peer, app.auth.client_address(peer.ip(), &headers), &headers);
    // As in Moonraker, a websocket with a wrong key or token still opens, with no rights.
    let refused = |e: ApiError| {
        let access = refused_access(&app, peer, &headers);
        app.auth.log_refusal(&access, "the websocket", &e);
        access
    };
    let base = base_access(&app, peer, &headers).unwrap_or_else(refused);
    let base = with_oneshot(&app, base, query.as_deref());
    let base = match with_proxy_user(&app, peer, &headers, base.clone()) {
        Ok(access) => access,
        Err(e) => {
            refused(e);
            base
        }
    };
    let user = token_user(&app, &headers, query.as_deref()).unwrap_or_else(|e| {
        refused(e);
        None
    });
    ws.on_upgrade(move |socket| serve_socket(app, socket, from, base, user))
}

/// What a TCP request may do: a key's rights, when it sends `X-Api-Key`, or else a trusted
/// network's, by the client's address, as long as a browser's `Origin` is allowed. A token adds
/// its user's rights. A key or token that isn't one is an error, as in Moonraker.
pub fn request_access(
    app: &App,
    peer: SocketAddr,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<crate::auth::Access, ApiError> {
    let base = with_oneshot(app, base_access(app, peer, headers)?, query);
    let base = with_proxy_user(app, peer, headers, base)?;
    Ok(match token_user(app, headers, query)? {
        Some((name, role)) => base.with_user(&name, role),
        None => base,
    })
}

/// A request's access, or a one-shot token's when `?token=` gives more, as in Moonraker. A
/// token that isn't one is ignored, as in Moonraker: the request keeps its own rights.
fn with_oneshot(app: &App, access: crate::auth::Access, query: Option<&str>) -> crate::auth::Access {
    let token = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(k, _)| k == "token")
        .last()
        .map(|(_, v)| v.into_owned());
    let Some(granted) = token.and_then(|t| app.auth.use_oneshot(t.trim(), &access.client)) else { return access };
    if granted.role > access.role { granted } else { access }
}

/// A request's access with the user a trusted proxy logged in, if it names one. A user that
/// `orrery.toml` gives no role is an error, as for a password login.
fn with_proxy_user(
    app: &App,
    peer: SocketAddr,
    headers: &HeaderMap,
    access: crate::auth::Access,
) -> Result<crate::auth::Access, ApiError> {
    let Some(name) = app.auth.proxy_user(peer.ip(), headers) else { return Ok(access) };
    crate::users::check_name(&name).map_err(|e| ApiError::new(403, format!("The proxy's user name won't do: {e}")))?;
    let role = app.users.role_of(&name)?;
    let access = access.with_proxy_user(&name, role);
    if app.auth.first_time(format!("proxy-user {name}")) {
        tracing::info!(target: "orrery::auth", "Proxy user {name} came in, as {}", access.requester());
    }
    Ok(access)
}

/// A request's access without its token.
fn base_access(app: &App, peer: SocketAddr, headers: &HeaderMap) -> Result<crate::auth::Access, ApiError> {
    let mut access = match headers.get("x-api-key").and_then(|v| v.to_str().ok()).filter(|k| !k.trim().is_empty()) {
        Some(key) => app.keys.lookup(key.trim()).ok_or_else(|| ApiError::new(401, "Invalid API Key"))?,
        None => network_access(app, peer, headers),
    };
    access.client = app.auth.client_address(peer.ip(), headers).to_string();
    Ok(access)
}

/// The user of a request's access token, and their role. The token can come as Moonraker
/// takes it: `Authorization: Bearer`, `X-Access-Token`, or `?access_token=`.
fn token_user(
    app: &App,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Option<(String, crate::auth::Role)>, ApiError> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    let token = match header("authorization") {
        // Another kind of Authorization, such as a proxy's Basic, isn't orrery's.
        Some(value) => value.strip_prefix("Bearer ").map(str::to_owned),
        None => header("x-access-token").map(str::to_owned).or_else(|| {
            form_urlencoded::parse(query.unwrap_or_default().as_bytes())
                .filter(|(k, _)| k == "access_token")
                .last()
                .map(|(_, v)| v.into_owned())
        }),
    };
    let Some(token) = token.filter(|t| !t.trim().is_empty()) else { return Ok(None) };
    let name = app.users.check_token(token.trim(), crate::users::TokenType::Access, true)?;
    let role = app.users.role_of(&name)?;
    Ok(Some((name, role)))
}

/// No rights, for a request whose key isn't one, with the client named for the log.
pub fn refused_access(app: &App, peer: SocketAddr, headers: &HeaderMap) -> crate::auth::Access {
    crate::auth::Access {
        client: app.auth.client_address(peer.ip(), headers).to_string(),
        ..crate::auth::Access::none("a wrong API key")
    }
}

fn network_access(app: &App, peer: SocketAddr, headers: &HeaderMap) -> crate::auth::Access {
    let client = app.auth.client_address(peer.ip(), headers);
    let origin_ok = crate::auth::origin_allowed(headers, &app.cors);
    if !origin_ok && let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        app.auth.log_once(format!("origin {origin}"), || {
            format!("A request from a page at {origin} gets no rights from its network: the origin isn't the printer's own address or in cors_domains")
        });
    }
    app.auth.network_access(client, origin_ok)
}

/// Where a connection comes from, for the log: the client's address, the proxy's address when a
/// reverse proxy such as nginx or caddy forwarded it, and the client's `User-Agent`. This helps
/// tell apart clients that never call `server.connection.identify`.
/// Who a connection is, for its log line: the client's address as orrery judged it, and the
/// proxy it came through when a trusted proxy passed that address on.
fn describe_peer(peer: SocketAddr, client: std::net::IpAddr, headers: &HeaderMap) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let mut from = if client == peer.ip().to_canonical() { peer.to_string() } else { format!("{client} (via {peer})") };
    if let Some(agent) = header("user-agent") {
        from.push_str(&format!(", User Agent: {agent}"));
    }
    from
}

async fn serve_socket(
    app: Arc<App>,
    socket: WebSocket,
    from: String,
    access: crate::auth::Access,
    user: Option<(String, crate::auth::Role)>,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = app.add_client(tx.clone(), crate::server::Peer::default(), access);
    if let Some((name, role)) = user {
        app.set_client_user(conn, &name, role);
    }
    let described = app.client_access(conn).describe();
    let opened = Instant::now();
    tracing::info!("Websocket Opened: ID: {conn}, From: {from}, Access: {described}");
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
        if app.take_reply(&text, conn) {
            continue;
        }
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let access = match request_access(&app, peer, &headers, query.as_deref()) {
        Ok(a) => a,
        Err(e) => {
            let access = refused_access(&app, peer, &headers);
            app.auth.log_refusal(&access, &format!("{method} {}", uri.path()), &e);
            return error_response(e.status, &e.message);
        }
    };
    // OctoPrint's file paths carry the file in the path, as Moonraker's pattern
    // `/api/files/moonraker/(?P<relative_path>.+)` does.
    let octoprint_file = uri
        .path()
        .strip_prefix(crate::octoprint::FILES_PREFIX)
        .filter(|rest| !rest.is_empty() && app.octoprint.is_some())
        .map(|rest| percent_encoding::percent_decode_str(rest).decode_utf8_lossy().into_owned());
    let found = match &octoprint_file {
        Some(_) => app.find_path(crate::octoprint::FILES_ENDPOINT).await,
        None => app.find_path(uri.path()).await,
    };
    let Some(endpoint) = found.filter(|e| e.http) else {
        if let Some(rest) = uri.path().strip_prefix("/server/files/") {
            let kind = if method == Method::DELETE { RequestType::Delete } else { RequestType::Get };
            let rest = percent_encoding::percent_decode_str(rest).decode_utf8_lossy().into_owned();
            let path = format!("/server/files/{rest}");
            if let Err(e) = crate::auth::authorize(&app, &access, &path, kind, &Args::default()) {
                app.auth.log_refusal(&access, &format!("{method} {}", uri.path()), &e);
                return error_response(e.status, &e.message);
            }
        }
        if let Some(resp) = crate::files::http::file_request(&app, &method, uri.path(), uri.query(), &headers).await {
            if method == Method::DELETE
                && let Some(rest) = uri.path().strip_prefix("/server/files/")
            {
                let rest = percent_encoding::percent_decode_str(rest).decode_utf8_lossy().into_owned();
                let args = Args([("path".to_owned(), json!(rest))].into_iter().collect());
                let failed = format!("status {}", resp.status().as_u16());
                let outcome = if resp.status().is_success() { Ok(()) } else { Err(failed.as_str()) };
                crate::auth::audit(
                    &app,
                    &access,
                    &format!("/server/files/{rest}"),
                    RequestType::Delete,
                    &args,
                    outcome,
                );
            }
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
    if let Some(file) = octoprint_file {
        args.insert("relative_path".into(), json!(file));
    }
    let args = Args(args);
    if let Err(e) = crate::auth::authorize(&app, &access, &endpoint.endpoint, kind, &args) {
        app.auth.log_refusal(&access, &format!("{method} {}", uri.path()), &e);
        return error_response(e.status, &e.message);
    }
    let audit = (kind != RequestType::Get).then(|| (args.clone(), access.clone()));
    let req =
        Request { endpoint: endpoint.endpoint.clone(), kind, args, transport: Transport::Http, conn: None, access };
    let result = (endpoint.handler)(app.clone(), req).await;
    if let Some((args, access)) = audit {
        let outcome = result.as_ref().map(|_| ()).map_err(|e| e.message.as_str());
        crate::auth::audit(&app, &access, &endpoint.endpoint, kind, &args, outcome);
    }
    match result {
        // As Moonraker's `wrap_result=False`: nothing is 204, and the rest is sent as it is.
        Ok(Value::Null) if endpoint.bare => StatusCode::NO_CONTENT.into_response(),
        Ok(result) if endpoint.bare => {
            ([(header::CONTENT_TYPE, "application/json; charset=UTF-8")], result.to_string()).into_response()
        }
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
        let local = peer.ip();
        let mut headers = HeaderMap::new();
        assert_eq!(describe_peer(peer, local, &headers), "127.0.0.1:59894");
        headers.insert("x-forwarded-for", "192.168.1.199, 10.0.0.1".parse().unwrap());
        headers.insert("user-agent", "HomeAssistant/2026.11 aiohttp/3.12".parse().unwrap());
        // From a trusted proxy, the address it added counts.
        let client = "10.0.0.1".parse().unwrap();
        assert_eq!(
            describe_peer(peer, client, &headers),
            "10.0.0.1 (via 127.0.0.1:59894), User Agent: HomeAssistant/2026.11 aiohttp/3.12"
        );
        // From anyone else, the headers are ignored, and so the log doesn't repeat them.
        assert_eq!(
            describe_peer(peer, local, &headers),
            "127.0.0.1:59894, User Agent: HomeAssistant/2026.11 aiohttp/3.12"
        );
    }
}
