//! The Spoolman server: its HTTP API, and the websocket where it reports changes to spools.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;

/// A Spoolman server, from the `server` option of `[spoolman]`.
pub struct Server {
    /// Such as `http://spoolman.local:7912/api`.
    pub api: String,
    /// Such as `ws://spoolman.local:7912/api/v1/spool`.
    pub ws: String,
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
}

/// A reply from Spoolman.
pub struct Response {
    pub status: u16,
    pub headers: Map<String, Value>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn has_error(&self) -> bool {
        self.status >= 400
    }

    /// Such as "Not Found".
    pub fn reason(&self) -> &'static str {
        reqwest::StatusCode::from_u16(self.status).ok().and_then(|s| s.canonical_reason()).unwrap_or("Unknown")
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// Spoolman's own error message, if the reply has one.
    pub fn message(&self) -> Option<String> {
        self.json()?.get("message")?.as_str().map(str::to_owned)
    }

    /// The error for a log line, such as "HTTP 404 Not Found: No spool with ID 9 found.".
    pub fn error_text(&self) -> String {
        let mut text = format!("HTTP {} {}", self.status, self.reason());
        if let Some(message) = self.message() {
            text.push_str(&format!(": {message}"));
        }
        text
    }
}

/// A websocket connection, plain or over TLS.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Socket = WebSocketStream<Box<dyn Io>>;

impl Server {
    /// Builds the URLs as Moonraker does: `http://` unless the option says `https://`, and the
    /// API under `/api`.
    pub fn new(server: &str) -> Result<Self, String> {
        let (scheme, host) = match server.split_once("://") {
            Some((s, h)) if s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https") => {
                (s.to_ascii_lowercase(), h)
            }
            _ => ("http".to_owned(), server),
        };
        let host = host.trim_end_matches('/');
        if host.is_empty() {
            return Err(format!("[spoolman] server: {server:?} isn't a URL"));
        }
        let ws_scheme = if scheme == "https" { "wss" } else { "ws" };
        let tls = Arc::new(tls_config()?);
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured((*tls).clone())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("can't set up the HTTP client: {e}"))?;
        Ok(Self { api: format!("{scheme}://{host}/api"), ws: format!("{ws_scheme}://{host}/api/v1/spool"), http, tls })
    }

    /// Sends a request to `path` under the API, such as `/v1/spool/3`. An error is a failure to
    /// get any reply, with its reason.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<Response, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, format!("{}{path}", self.api));
        if let Some(body) = body {
            req = req.json(body);
        }
        if let Some(timeout) = timeout {
            req = req.timeout(timeout);
        }
        let resp = req.send().await.map_err(error_chain)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (header_case(k.as_str()), json!(String::from_utf8_lossy(v.as_bytes()))))
            .collect();
        let body = resp.bytes().await.map_err(error_chain)?.to_vec();
        Ok(Response { status, headers, body })
    }

    /// Opens the websocket where Spoolman reports changes to spools.
    pub async fn connect_ws(&self) -> Result<Socket, String> {
        let url = reqwest::Url::parse(&self.ws).map_err(|e| format!("{}: {e}", self.ws))?;
        let host = url.host_str().ok_or_else(|| format!("{}: no host", self.ws))?.to_owned();
        let port = url.port_or_known_default().unwrap_or(80);
        let attempt = async {
            let tcp = TcpStream::connect((host.trim_matches(['[', ']']), port)).await.map_err(|e| e.to_string())?;
            let stream: Box<dyn Io> = if url.scheme() == "wss" {
                let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
                let connector = tokio_rustls::TlsConnector::from(self.tls.clone());
                Box::new(connector.connect(name, tcp).await.map_err(|e| e.to_string())?)
            } else {
                Box::new(tcp)
            };
            let (socket, _) =
                tokio_tungstenite::client_async(self.ws.as_str(), stream).await.map_err(|e| e.to_string())?;
            Ok(socket)
        };
        tokio::time::timeout(Duration::from_secs(5), attempt).await.map_err(|_| "timed out".to_owned())?
    }
}

/// TLS with a pure-Rust crypto provider, and the system's trusted certificates.
fn tls_config() -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls_rustcrypto::provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|e| format!("can't load the system's certificates: {e}"))?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// An error with its causes, which hold the useful part, such as "Connection refused".
fn error_chain(e: reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        text.push_str(&format!(": {s}"));
        source = s.source();
    }
    text
}

/// A header name as Moonraker's HTTP client reports it, such as `X-Total-Count`.
fn header_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_follow_moonraker() {
        let s = Server::new("spoolman.local:7912/").unwrap();
        assert_eq!(
            (s.api.as_str(), s.ws.as_str()),
            ("http://spoolman.local:7912/api", "ws://spoolman.local:7912/api/v1/spool")
        );
        let s = Server::new("HTTPS://spoolman.example").unwrap();
        assert_eq!(
            (s.api.as_str(), s.ws.as_str()),
            ("https://spoolman.example/api", "wss://spoolman.example/api/v1/spool")
        );
        assert!(Server::new("http://").is_err());
    }

    #[test]
    fn header_names() {
        assert_eq!(header_case("x-total-count"), "X-Total-Count");
        assert_eq!(header_case("content-type"), "Content-Type");
    }
}
