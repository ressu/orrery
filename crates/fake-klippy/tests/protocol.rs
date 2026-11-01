//! Checks the fake against the Klipper behaviour it copies (klippy/webhooks.py).

use std::collections::VecDeque;
use std::path::Path;

use fake_klippy::script::ScriptFile;
use fake_klippy::{FakeKlippy, KlippyState, Script};
use orrery_proto::klippy;
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const SCRIPT: &str = r#"
objects:
  toolhead: {position: [0, 0, 0, 0], homed_axes: ""}
  print_stats: {state: standby}
endpoints: [objects/list, objects/query, objects/subscribe, gcode/script, gcode/subscribe_output]
gcode:
  - match: '^SAY (?P<word>\w+)$'
    respond: ["// {word}"]
  - match: '^BREAK$'
    error: broken
"#;

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    /// Messages without our reply id that arrived while a call waited.
    pending: VecDeque<Value>,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let (r, w) = UnixStream::connect(path).await.unwrap().into_split();
        Self { reader: BufReader::new(r), writer: w, next_id: 0, pending: VecDeque::new() }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        klippy::write_message(&mut self.writer, &json!({"id": id, "method": method, "params": params})).await.unwrap();
        loop {
            let msg = self.read().await;
            if msg["id"] == json!(id) {
                return msg;
            }
            self.pending.push_back(msg);
        }
    }

    /// The next message that isn't a reply to a call.
    async fn next(&mut self) -> Value {
        match self.pending.pop_front() {
            Some(msg) => msg,
            None => self.read().await,
        }
    }

    async fn read(&mut self) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(2), klippy::read_message(&mut self.reader))
            .await
            .expect("no message within 2s")
            .unwrap()
            .expect("socket closed")
    }
}

async fn start() -> (tempfile::TempDir, FakeKlippy) {
    let dir = tempfile::tempdir().unwrap();
    let file: ScriptFile = serde_yaml_ng::from_str(SCRIPT).unwrap();
    let script = Script::from_script_file(file, Path::new(".")).unwrap();
    let fake = FakeKlippy::start(dir.path().join("klippy.sock"), script).await.unwrap();
    (dir, fake)
}

#[tokio::test]
async fn query_returns_null_for_missing_fields_and_empty_for_unknown_objects() {
    let (_dir, fake) = start().await;
    let mut c = Client::connect(fake.socket_path()).await;
    let reply =
        c.call("objects/query", json!({"objects": {"toolhead": ["homed_axes", "nope"], "nothing": null}})).await;
    assert_eq!(reply["result"]["status"], json!({"toolhead": {"homed_axes": "", "nope": null}, "nothing": {}}));
    assert!(reply["result"]["eventtime"].is_number());
}

#[tokio::test]
async fn subscribers_get_only_changed_fields_in_their_template() {
    let (_dir, fake) = start().await;
    let mut c = Client::connect(fake.socket_path()).await;
    let reply = c
        .call(
            "objects/subscribe",
            json!({"objects": {"toolhead": null}, "response_template": {"method": "process_status_update"}}),
        )
        .await;
    assert_eq!(reply["result"]["status"]["toolhead"]["homed_axes"], "");

    let patch =
        json!({"toolhead": {"homed_axes": "xyz", "position": [0, 0, 0, 0]}, "print_stats": {"state": "printing"}});
    fake.update_objects(patch.as_object().unwrap());
    let update = c.next().await;
    assert_eq!(update["method"], "process_status_update");
    // position didn't change and print_stats isn't subscribed.
    assert_eq!(update["params"]["status"], json!({"toolhead": {"homed_axes": "xyz"}}));
}

#[tokio::test]
async fn gcode_rules_respond_and_fail_like_klipper() {
    let (_dir, fake) = start().await;
    let mut c = Client::connect(fake.socket_path()).await;
    c.call("gcode/subscribe_output", json!({"response_template": {"method": "process_gcode_response"}})).await;

    let ok = c.call("gcode/script", json!({"script": "SAY hello"})).await;
    assert_eq!(ok["result"], json!({}));
    // The output arrives before the reply, as in Klipper.
    assert_eq!(c.next().await, json!({"method": "process_gcode_response", "params": {"response": "// hello"}}));

    let err = c.call("gcode/script", json!({"script": "BREAK"})).await;
    assert_eq!(err["error"], json!({"error": "WebRequestError", "message": "broken"}));

    let missing = c.call("bed_mesh/dump_mesh", json!({})).await;
    assert_eq!(missing["error"]["message"], "webhooks: No registered callback for path 'bed_mesh/dump_mesh'");
}

#[tokio::test]
async fn emergency_stop_updates_webhooks_and_blocks_gcode() {
    let (_dir, fake) = start().await;
    let mut c = Client::connect(fake.socket_path()).await;
    c.call(
        "objects/subscribe",
        json!({"objects": {"webhooks": null}, "response_template": {"method": "process_status_update"}}),
    )
    .await;
    c.call("emergency_stop", json!({})).await;
    let update = c.next().await;
    assert_eq!(update["params"]["status"]["webhooks"]["state"], "shutdown");
    assert_eq!(fake.state(), KlippyState::Shutdown);

    let refused = c.call("gcode/script", json!({"script": "G28"})).await;
    assert!(refused.get("error").is_some());
}

#[tokio::test]
async fn restart_drops_clients_then_becomes_ready() {
    let (_dir, fake) = start().await;
    let mut c = Client::connect(fake.socket_path()).await;
    c.call("info", json!({})).await;
    fake.restart();
    assert_eq!(fake.state(), KlippyState::Startup);
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), klippy::read_message(&mut c.reader)).await;
    assert!(matches!(closed, Ok(Ok(None))), "client should be disconnected");
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(fake.state(), KlippyState::Ready);
}
