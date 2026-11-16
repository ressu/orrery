//! The agent between a stand-in for orrery and a stand-in for Spoolman: it reports the filament
//! used, and clears the active spool when Spoolman deletes it.

use std::time::Duration;

use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// What the stand-ins saw.
#[derive(Debug)]
enum Seen {
    /// A call the agent made to orrery.
    Call(String, Value),
    /// A request the agent sent to Spoolman's API.
    Http(String, String, Value),
}

/// Spoolman: every request gets `{}`, and the websocket sends what `events` gets.
async fn fake_spoolman(
    listener: TcpListener,
    seen: mpsc::UnboundedSender<Seen>,
    events: mpsc::UnboundedReceiver<String>,
) {
    let mut events = Some(events);
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let mut head = vec![0u8; 4096];
        let n = stream.peek(&mut head).await.unwrap();
        let text = String::from_utf8_lossy(&head[..n]).to_ascii_lowercase();
        if text.contains("upgrade: websocket") {
            let mut events = events.take().expect("one websocket");
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(event) = events.recv().await {
                    ws.send(Message::text(event)).await.unwrap();
                }
            });
            continue;
        }
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let mut parts = line.split_whitespace();
        let (method, path) = (parts.next().unwrap().to_owned(), parts.next().unwrap().to_owned());
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).await.unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await.unwrap();
        let body = serde_json::from_slice(&body).unwrap_or_default();
        seen.send(Seen::Http(method, path, body)).unwrap();
        let reply =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        reader.into_inner().write_all(reply.as_bytes()).await.unwrap();
    }
}

async fn send(writer: &mut OwnedWriteHalf, msg: Value) {
    let mut data = msg.to_string().into_bytes();
    data.push(0x03);
    writer.write_all(&data).await.unwrap();
}

/// Waits for the next thing seen that `want` accepts, skipping the rest.
async fn expect(rx: &mut mpsc::UnboundedReceiver<Seen>, want: impl Fn(&Seen) -> bool) -> Seen {
    let wait = async {
        loop {
            let seen = rx.recv().await.unwrap();
            if want(&seen) {
                return seen;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait).await.expect("timed out")
}

#[tokio::test]
async fn reports_use_and_follows_deleted_spools() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("moonraker.sock");
    let orrery = UnixListener::bind(&socket).unwrap();
    let spoolman = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", spoolman.local_addr().unwrap());
    let (seen_tx, mut seen) = mpsc::unbounded_channel();
    let (events, events_rx) = mpsc::unbounded_channel();
    tokio::spawn(fake_spoolman(spoolman, seen_tx.clone(), events_rx));

    let mut agent = tokio::process::Command::new(env!("CARGO_BIN_EXE_orrery-spoolman"))
        .arg(&socket)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = orrery.accept().await.unwrap();
    let (reader, mut writer) = stream.into_split();

    // orrery: answers the agent's calls, and passes on what the test sends.
    let (to_agent, mut outbox) = mpsc::unbounded_channel::<Value>();
    let seen_calls = seen_tx.clone();
    let replies_to = to_agent.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            let mut buf = Vec::new();
            if reader.read_until(0x03, &mut buf).await.unwrap() == 0 {
                return;
            }
            buf.pop();
            let msg: Value = serde_json::from_slice(&buf).unwrap();
            let Some(method) = msg["method"].as_str() else { continue };
            let params = msg.get("params").cloned().unwrap_or_default();
            let result = match method {
                "orrery.agent.config" => json!({"exists": true, "options": {"server": url, "sync_rate": 1}}),
                "server.database.get_item" => {
                    let error = json!({"code": 404, "message": "Key not found"});
                    replies_to.send(json!({"jsonrpc": "2.0", "error": error, "id": msg["id"]})).unwrap();
                    continue;
                }
                "server.info" => json!({"klippy_state": "ready"}),
                "printer.objects.subscribe" => {
                    json!({"status": {"toolhead": {"position": [0, 0, 0, 10.0], "extruder": "extruder"}}})
                }
                "server.history.list" => json!({"jobs": []}),
                _ => json!("ok"),
            };
            seen_calls.send(Seen::Call(method.to_owned(), params)).unwrap();
            replies_to.send(json!({"jsonrpc": "2.0", "result": result, "id": msg["id"]})).unwrap();
        }
    });
    tokio::spawn(async move {
        while let Some(msg) = outbox.recv().await {
            send(&mut writer, msg).await;
        }
    });

    // The agent finishes its setup, and opens the websocket to Spoolman, in either order.
    let (mut set_up, mut connected) = (false, false);
    while !(set_up && connected) {
        match expect(&mut seen, |_| true).await {
            Seen::Call(m, _) if m == "server.history.list" => set_up = true,
            Seen::Call(m, p) if m == "connection.send_event" && p["data"]["spoolman_connected"] == true => {
                connected = true
            }
            _ => {}
        }
    }

    let status = |e: f64| json!({"jsonrpc": "2.0", "method": "notify_status_update", "params": [{"toolhead": {"position": [0, 0, 0, e]}}, 1.0]});
    to_agent.send(status(10.0)).unwrap();
    to_agent.send(json!({"jsonrpc": "2.0", "method": "post_spool_id", "params": {"spool_id": 7}, "id": 1})).unwrap();
    let saved = expect(&mut seen, |s| matches!(s, Seen::Call(m, _) if m == "server.database.post_item")).await;
    assert!(matches!(saved, Seen::Call(_, p) if p == json!({"namespace": "spoolman", "key": "spool_id", "value": 7})));
    let event = expect(
        &mut seen,
        |s| matches!(s, Seen::Call(m, p) if m == "connection.send_event" && p["event"] == "active_spool_set"),
    )
    .await;
    assert!(matches!(event, Seen::Call(_, p) if p["data"] == json!({"spool_id": 7})));

    // 2.5 mm and then 1.5 mm more, reported together within a second.
    to_agent.send(status(12.5)).unwrap();
    to_agent.send(status(14.0)).unwrap();
    to_agent.send(status(13.0)).unwrap(); // a retraction isn't used filament
    let report = expect(&mut seen, |s| matches!(s, Seen::Http(..))).await;
    let Seen::Http(method, path, body) = report else { unreachable!() };
    assert_eq!((method.as_str(), path.as_str()), ("PUT", "/api/v1/spool/7/use"));
    assert_eq!(body, json!({"use_length": 4.0}));

    // Spoolman deletes the spool, so there's no active spool now.
    events.send(json!({"resource": "spool", "type": "deleted", "payload": {"id": 7}}).to_string()).unwrap();
    let event = expect(
        &mut seen,
        |s| matches!(s, Seen::Call(m, p) if m == "connection.send_event" && p["event"] == "active_spool_set"),
    )
    .await;
    assert!(matches!(event, Seen::Call(_, p) if p["data"] == json!({"spool_id": null})));

    agent.kill().await.unwrap();
}
