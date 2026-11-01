//! Tracks Klippy's `print_stats`, after Moonraker's `job_state.py`. History and the job queue
//! build on it.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::sync::broadcast;

use crate::server::{App, Event};

#[derive(Default)]
pub struct JobState {
    last_stats: Mutex<Map<String, Value>>,
}

impl JobState {
    /// The latest `print_stats` fields.
    pub fn last_stats(&self) -> Map<String, Value> {
        self.last_stats.lock().expect("job state lock").clone()
    }
}

pub fn start(app: &Arc<App>) {
    tokio::spawn(follow_events(app.clone(), app.events.subscribe()));
}

async fn follow_events(app: Arc<App>, mut rx: broadcast::Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(Event::KlippyReady) => {
                let mut sub = Map::new();
                sub.insert("print_stats".into(), Value::Null);
                match app.klippy.subscribe_host(&app, sub).await {
                    Ok(result) => {
                        let stats = result["status"]["print_stats"].as_object().cloned().unwrap_or_default();
                        tracing::info!(
                            "Job state initialized: {}",
                            stats.get("state").and_then(serde_json::Value::as_str).unwrap_or("unknown")
                        );
                        *app.job_state.last_stats.lock().expect("job state lock") = stats;
                    }
                    Err(e) => tracing::info!("Error subscribing to print_stats: {e}"),
                }
            }
            Ok(Event::HostStatus(status)) => {
                if let Some(Value::Object(ps)) = status.get("print_stats") {
                    let mut last = app.job_state.last_stats.lock().expect("job state lock");
                    for (k, v) in ps {
                        last.insert(k.clone(), v.clone());
                    }
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
