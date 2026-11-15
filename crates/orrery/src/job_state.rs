//! Tracks Klippy's `print_stats` and turns its state changes into job events, after Moonraker's
//! `job_state.py`. History hears about each event in order, from this task.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::sync::broadcast;

use crate::server::{App, Event};

/// Moonraker's `JobEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobEvent {
    #[default]
    Standby,
    Started,
    Paused,
    Resumed,
    Complete,
    Error,
    Cancelled,
}

impl JobEvent {
    fn from_state(state: &str) -> Option<Self> {
        Some(match state {
            "standby" => Self::Standby,
            "started" => Self::Started,
            "paused" => Self::Paused,
            "resumed" => Self::Resumed,
            "complete" => Self::Complete,
            "error" => Self::Error,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn is_printing(self) -> bool {
        matches!(self, Self::Started | Self::Resumed)
    }

    pub fn aborted(self) -> bool {
        matches!(self, Self::Error | Self::Cancelled)
    }
}

#[derive(Default)]
pub struct JobState {
    last_stats: Mutex<Map<String, Value>>,
    last_event: Mutex<JobEvent>,
}

impl JobState {
    /// The latest `print_stats` fields.
    pub fn last_stats(&self) -> Map<String, Value> {
        self.last_stats.lock().expect("job state lock").clone()
    }

    pub fn last_event(&self) -> JobEvent {
        *self.last_event.lock().expect("job state lock")
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
                    status_update(&app, ps).await;
                }
            }
            Ok(Event::KlippyShutdown) => crate::history::finish_job(&app, "klippy_shutdown", None).await,
            Ok(Event::KlippyDisconnected) => {
                {
                    let mut last = app.job_state.last_stats.lock().expect("job state lock");
                    if matches!(last.get("state").and_then(Value::as_str), Some("printing" | "paused")) {
                        last.insert("state".into(), "error".into());
                        *app.job_state.last_event.lock().expect("job state lock") = JobEvent::Error;
                    }
                }
                crate::history::finish_job(&app, "klippy_disconnect", None).await;
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Moonraker's `_status_update`: works out which job event a state change means.
async fn status_update(app: &Arc<App>, ps: &Map<String, Value>) {
    let prev = app.job_state.last_stats();
    let mut new = prev.clone();
    for (k, v) in ps {
        new.insert(k.clone(), v.clone());
    }
    *app.job_state.last_stats.lock().expect("job state lock") = new.clone();
    let Some(new_state) = ps.get("state").and_then(Value::as_str) else { return };
    let old_state = prev.get("state").and_then(Value::as_str).unwrap_or_default();
    if new_state == old_state {
        return;
    }
    let mut event = new_state;
    if is_idle(old_state) {
        if new_state == "printing" {
            event = "started";
        } else if new_state != "standby" {
            tracing::info!("Job started event inferred: transition from {old_state} to {new_state} state");
            send_job_event(app, "started", &prev, &new).await;
        }
    } else if new_state == "printing" {
        event = if is_resumed(&prev, &new) { "resumed" } else { "started" };
    }
    tracing::debug!("Job State Changed - Prev State: {old_state}, New State: {event}");
    send_job_event(app, event, &prev, &new).await;
}

fn is_idle(state: &str) -> bool {
    matches!(state, "standby" | "complete" | "cancelled" | "error")
}

fn is_resumed(prev: &Map<String, Value>, new: &Map<String, Value>) -> bool {
    let duration = |m: &Map<String, Value>| m.get("total_duration").and_then(Value::as_f64).unwrap_or(0.0);
    prev.get("state").and_then(Value::as_str) == Some("paused")
        && prev.get("filename") == new.get("filename")
        && duration(prev) < duration(new)
}

async fn send_job_event(app: &Arc<App>, state: &str, prev: &Map<String, Value>, new: &Map<String, Value>) {
    if state == "started" {
        tracing::info!("Job Started: {}", new.get("filename").and_then(serde_json::Value::as_str).unwrap_or_default());
    }
    let Some(event) = JobEvent::from_state(state) else { return };
    *app.job_state.last_event.lock().expect("job state lock") = event;
    tokio::spawn(crate::job_queue::on_job_event(app.clone(), event));
    crate::history::on_job_event(app, event, prev, new).await;
}
