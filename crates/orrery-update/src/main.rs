//! orrery-update: Moonraker's update manager as an orrery agent
//! (docs/design/2026-11-22-update-manager.md).
//!
//! orrery starts it when `[update_manager]`, or one of its entries, is in `orrery.toml` or, as
//! the fallback, in `moonraker.conf`. It answers Moonraker's `machine.update.*` methods, which
//! orrery maps to `agent.update_manager.*`, and sends `notify_update_refreshed` and
//! `notify_update_response`. It keeps each entry's state in orrery's database, in the
//! `update_manager` namespace.

mod config;
mod deps;
mod git;
mod manager;
mod packagekit;
mod version;

use orrery_agent::{AgentBuilder, MethodError};

use manager::Manager;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    orrery_agent::init_logging();
    let manager = Manager::new();
    let mut builder = AgentBuilder::new(config::SECTION).version(env!("CARGO_PKG_VERSION"));
    if let Some(socket) = std::env::args().nth(1) {
        builder = builder.socket(socket);
    }
    let m = manager.clone();
    builder = builder.method("status", move |params| {
        let m = m.clone();
        async move { m.status(&params).await }
    });
    let m = manager.clone();
    builder = builder.method("refresh", move |params| {
        let m = m.clone();
        async move { m.refresh(&params).await }
    });
    for name in ["upgrade", "client", "full"] {
        let m = manager.clone();
        builder = builder.method(name, move |params| {
            let m = m.clone();
            // `full` has no name: it updates everything.
            let params = if name == "full" { serde_json::Value::Null } else { params };
            async move { m.update(&params).await }
        });
    }
    let m = manager.clone();
    builder = builder.method("recover", move |params| {
        let m = m.clone();
        async move { m.recover(&params).await }
    });
    let m = manager.clone();
    builder = builder.method("rollback", move |params| {
        let m = m.clone();
        async move { m.rollback(&params).await }
    });
    // Debian packages come later.
    builder = builder.method("system", |_| async {
        Err::<serde_json::Value, _>(MethodError::new(404, "Updater system not available"))
    });
    // Moonraker's own entries, which orrery doesn't have (see the design note).
    builder = builder.method("klipper", |_| async {
        Err::<serde_json::Value, _>(MethodError::new(400, "Klipper updates aren't available in orrery yet."))
    });
    builder = builder.method("moonraker", |_| async {
        Err::<serde_json::Value, _>(MethodError::new(400, "orrery doesn't update Moonraker."))
    });
    orrery_agent::run(builder, move |agent| {
        let m = manager.clone();
        async move { m.setup(agent).await }
    })
    .await
}
