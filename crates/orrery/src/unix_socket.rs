//! The API on a unix socket, `<data>/comms/moonraker.sock`, after Moonraker's
//! `extensions.UnixSocketClient`. Every JSON-RPC method the websocket has works here too. Each
//! message is UTF-8 JSON followed by an ETX byte (`0x03`), in both directions, as on Klippy's
//! socket. Clients don't log in: the socket file's permissions decide who can connect.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::server::App;

const ETX: u8 = 0x03;

/// The longest message orrery reads, as in Moonraker. A longer one closes the connection.
const MAX_MESSAGE: usize = 20 * 1024 * 1024;

/// Messages that fail to decode in a row before orrery closes the connection, as in Moonraker.
const MAX_ERRORS: u32 = 10;

/// The socket file, removed when orrery stops.
pub struct UnixSocket {
    path: PathBuf,
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Opens the socket and accepts clients in the background. A socket file left by an earlier run
/// is replaced, but one that another program still listens on is left alone, with a warning.
pub async fn start(app: &Arc<App>, path: &Path) -> Result<Option<UnixSocket>> {
    tracing::info!("Creating Unix Domain Socket at '{}'", path.display());
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            app.add_warning(format!(
                "Another program is listening on '{}', so orrery's unix socket is off. \
                 Stop that program, or give orrery another socket with -u.",
                path.display()
            ));
            return Ok(None);
        }
        std::fs::remove_file(path).with_context(|| format!("removing the old socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("creating the unix socket {}", path.display()))?;
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve(app.clone(), stream));
                }
                Err(e) => tracing::warn!("Unix socket accept failed: {e}"),
            }
        }
    });
    Ok(Some(UnixSocket { path: path.to_path_buf() }))
}

async fn serve(app: Arc<App>, stream: UnixStream) {
    let cred = stream.peer_cred().ok();
    let (reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = app.add_client(tx.clone(), crate::server::Peer { unix: true, uid: cred.map(|c| c.uid()) });
    let opened = std::time::Instant::now();
    tracing::info!(
        "Unix Socket Opened - Client ID: {conn}, Process ID: {}, User ID: {}, Group ID: {}",
        cred.and_then(|c| c.pid()).map_or("unknown".into(), |p| p.to_string()),
        cred.map_or("unknown".into(), |c| c.uid().to_string()),
        cred.map_or("unknown".into(), |c| c.gid().to_string()),
    );
    let write_task = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let mut data = text.into_bytes();
            data.push(ETX);
            if writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    let reason = read_messages(&app, conn, BufReader::new(reader), tx).await;
    let name = app.client_name(conn);
    app.remove_client(conn);
    write_task.abort();
    tracing::info!(
        "Unix Socket Closed: ID: {conn}, Close Reason: {reason}, {}",
        crate::server::describe_close(name, opened)
    );
}

/// Reads messages until the client disconnects, and returns why the connection ended.
async fn read_messages(
    app: &Arc<App>,
    conn: crate::api::ConnId,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    tx: mpsc::UnboundedSender<String>,
) -> &'static str {
    let mut errors_left = MAX_ERRORS;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = (&mut reader).take(MAX_MESSAGE as u64 + 1).read_until(ETX, &mut buf).await;
        match read {
            Ok(0) => return "Read Exit",
            Ok(_) => {}
            Err(_) => return "Read Error",
        }
        if buf.last() != Some(&ETX) {
            // The client closed mid-message, or the message is too long.
            return if buf.len() > MAX_MESSAGE { "Message Too Long" } else { "Read Exit" };
        }
        buf.pop();
        let Ok(text) = String::from_utf8(std::mem::take(&mut buf)) else {
            tracing::warn!("Unix socket {conn}: a message isn't valid UTF-8");
            errors_left -= 1;
            if errors_left == 0 {
                return "Too Many Errors";
            }
            continue;
        };
        errors_left = MAX_ERRORS;
        // Requests run side by side, as on the websocket.
        let app = app.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(reply) = app.dispatch_jsonrpc(&text, Some(conn)).await {
                let _ = tx.send(reply.to_string());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::Paths;
    use serde_json::Value;

    async fn app(dir: &Path) -> Arc<App> {
        let paths = Paths { data: dir.to_path_buf(), config_file: dir.join("moonraker.conf"), log_file: None };
        App::new(Config::parse("[server]\n").unwrap(), paths).unwrap()
    }

    async fn read_reply(stream: &mut UnixStream) -> Value {
        let mut buf = Vec::new();
        loop {
            let b = stream.read_u8().await.unwrap();
            if b == ETX {
                return serde_json::from_slice(&buf).unwrap();
            }
            buf.push(b);
        }
    }

    #[tokio::test]
    async fn answers_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let server = app(dir.path()).await;
        let path = dir.path().join("moonraker.sock");
        // A file left by a run that crashed is replaced.
        std::fs::write(&path, "").unwrap();
        let socket = start(&server, &path).await.unwrap().unwrap();

        let mut client = UnixStream::connect(&path).await.unwrap();
        client.write_all(b"{\"jsonrpc\": \"2.0\", \"method\": \"server.websocket.id\", \"id\": 7}\x03").await.unwrap();
        let reply = read_reply(&mut client).await;
        assert_eq!(reply["id"], 7);
        assert!(reply["result"]["websocket_id"].is_u64());

        client.write_all(b"not json\x03").await.unwrap();
        assert_eq!(read_reply(&mut client).await["error"]["code"], -32700);

        // A second orrery leaves the live socket alone.
        let other_dir = tempfile::tempdir().unwrap();
        let other = app(other_dir.path()).await;
        assert!(start(&other, &path).await.unwrap().is_none());

        drop(socket);
        assert!(!path.exists());
    }
}
