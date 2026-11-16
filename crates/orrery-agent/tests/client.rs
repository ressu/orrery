//! The client against a scripted stand-in for orrery's unix socket.

use orrery_agent::{AgentBuilder, MethodError};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

struct Server {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Server {
    async fn recv(&mut self) -> Value {
        let mut buf = Vec::new();
        self.reader.read_until(0x03, &mut buf).await.unwrap();
        buf.pop();
        serde_json::from_slice(&buf).unwrap()
    }

    async fn send(&mut self, msg: Value) {
        let mut data = msg.to_string().into_bytes();
        data.push(0x03);
        self.writer.write_all(&data).await.unwrap();
    }

    /// Answers the next call with `result`, after checking its method.
    async fn answer(&mut self, method: &str, result: Value) -> Value {
        let call = self.recv().await;
        assert_eq!(call["method"], method, "{call}");
        self.send(json!({"jsonrpc": "2.0", "result": result, "id": call["id"]})).await;
        call
    }
}

#[tokio::test]
async fn talks_to_orrery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("moonraker.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let builder = AgentBuilder::new("hello").version("1.2").socket(&path).method("greet", |params| async move {
        let name = params["name"].as_str().ok_or_else(|| MethodError::invalid_params("name is missing"))?;
        Ok(json!({"greeting": format!("Hello, {name}!")}))
    });
    let client = tokio::spawn(builder.connect());
    let (stream, _) = listener.accept().await.unwrap();
    let (reader, writer) = stream.into_split();
    let mut server = Server { reader: BufReader::new(reader), writer };

    let identify = server.answer("server.connection.identify", json!({"connection_id": 1})).await;
    assert_eq!(identify["params"], json!({"client_name": "hello", "version": "1.2", "type": "agent", "url": ""}));
    let agent = client.await.unwrap().unwrap();

    // A call relayed from a client, and one with bad params.
    server.send(json!({"jsonrpc": "2.0", "method": "greet", "params": {"name": "Ada"}, "id": 7})).await;
    assert_eq!(server.recv().await, json!({"jsonrpc": "2.0", "result": {"greeting": "Hello, Ada!"}, "id": 7}));
    server.send(json!({"jsonrpc": "2.0", "method": "greet", "id": 8})).await;
    assert_eq!(server.recv().await["error"], json!({"code": -32602, "message": "name is missing"}));
    server.send(json!({"jsonrpc": "2.0", "method": "nope", "id": 9})).await;
    assert_eq!(server.recv().await["error"]["code"], -32601);

    // The agent's own calls.
    let a = agent.clone();
    let call = tokio::spawn(async move { a.register_endpoint("greet", &["GET"]).await });
    let sent = server.answer("orrery.agent.register_endpoint", json!("ok")).await;
    assert_eq!(sent["params"], json!({"path": "greet", "request_methods": ["GET"]}));
    call.await.unwrap().unwrap();

    let a = agent.clone();
    let call = tokio::spawn(async move { a.config(None).await });
    let sent =
        server.answer("orrery.agent.config", json!({"section": "hello", "exists": true, "options": {"x": 1}})).await;
    assert!(sent.get("params").is_none());
    assert_eq!(call.await.unwrap().unwrap(), Some(json!({"x": 1}).as_object().unwrap().clone()));

    // An error from orrery.
    let a = agent.clone();
    let call = tokio::spawn(async move { a.send_event("connected", None).await });
    let sent = server.recv().await;
    server
        .send(json!({"jsonrpc": "2.0", "error": {"code": 400, "message": "Event 'connected' is reserved"}, "id": sent["id"]}))
        .await;
    let err = call.await.unwrap().unwrap_err().to_string();
    assert_eq!(err, "connection.send_event failed: Event 'connected' is reserved (code 400)");

    // Notifications reach subscribers.
    let mut notifications = agent.notifications();
    server.send(json!({"jsonrpc": "2.0", "method": "notify_klippy_ready"})).await;
    assert_eq!(notifications.recv().await.unwrap().method, "notify_klippy_ready");

    // `close` ends the stream, so orrery sees the agent leave.
    let a = agent.clone();
    let closing = tokio::spawn(async move { a.close().await });
    let mut rest = Vec::new();
    assert_eq!(server.reader.read_to_end(&mut rest).await.unwrap(), 0);
    drop(server);
    closing.await.unwrap();
    assert!(matches!(agent.call("server.info", Value::Null).await, Err(orrery_agent::Error::Closed)));
}

#[tokio::test]
async fn notices_orrery_leaving() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("moonraker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let client = tokio::spawn(AgentBuilder::new("hello").socket(&path).connect());
    let (stream, _) = listener.accept().await.unwrap();
    let (reader, writer) = stream.into_split();
    let mut server = Server { reader: BufReader::new(reader), writer };
    server.answer("server.connection.identify", json!({"connection_id": 1})).await;
    let agent = client.await.unwrap().unwrap();

    // When orrery goes away, calls fail and `closed` returns.
    drop(server);
    agent.closed().await;
    assert!(matches!(agent.call("server.info", Value::Null).await, Err(orrery_agent::Error::Closed)));
}
