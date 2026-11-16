//! A small agent: `agent.hello.greet` answers clients, a Klippy macro can call `hello_ping`,
//! and each ping is sent on to clients as an event.
//!
//! ```
//! cargo run -p orrery-agent --example hello -- ~/printer_data/comms/moonraker.sock
//! ```

use orrery_agent::{Agent, AgentBuilder, MethodError};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt().init();
    let mut builder =
        AgentBuilder::new("hello").version(env!("CARGO_PKG_VERSION")).method("greet", |params| async move {
            let name = params["name"].as_str().ok_or_else(|| MethodError::invalid_params("name is missing"))?;
            Ok(json!({"greeting": format!("Hello, {name}!")}))
        });
    if let Some(socket) = std::env::args().nth(1) {
        builder = builder.socket(socket);
    }
    orrery_agent::run(builder, setup).await
}

async fn setup(agent: Agent) -> Result<(), orrery_agent::Error> {
    agent.register_endpoint("greet", &["GET"]).await?;
    agent.register_remote_method("hello_ping").await?;
    let events = agent.clone();
    let mut notifications = agent.notifications();
    tokio::spawn(async move {
        while let Ok(n) = notifications.recv().await {
            if n.method == "hello_ping" {
                let _ = events.send_event("ping", Some(n.params)).await;
            }
        }
    });
    if let Some(config) = agent.config(None).await? {
        tracing::info!("My config: {config:?}");
    }
    Ok(())
}
