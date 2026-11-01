//! Recent gcode traffic and heater history, after Moonraker's `data_store.py`:
//! `server.gcode_store` and `server.temperature_store`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;

use crate::api::{ApiResult, Request, RequestType, Transports, handler};
use crate::config::Config;
use crate::server::{App, Event};

const TEMP_UPDATE_TIME: Duration = Duration::from_secs(1);
const TEMP_FIELDS: &[&str] = &["temperature", "target", "power", "speed"];

/// A sensor's history: each field with its samples.
type SensorStore = Vec<(String, VecDeque<Value>)>;

pub struct DataStore {
    temp_store_size: usize,
    gcode_store_size: usize,
    gcode: Mutex<VecDeque<Value>>,
    temperatures: Mutex<Vec<(String, SensorStore)>>,
    monitors: Mutex<Vec<String>>,
}

impl DataStore {
    pub fn new(config: &Config) -> Result<Self> {
        let section = config.section("data_store");
        let temp_store_size = section.get_int("temperature_store_size", 1200)?.max(1) as usize;
        let gcode_store_size = section.get_int("gcode_store_size", 1000)?.max(1) as usize;
        Ok(Self {
            temp_store_size,
            gcode_store_size,
            gcode: Mutex::new(VecDeque::new()),
            temperatures: Mutex::new(Vec::new()),
            monitors: Mutex::new(Vec::new()),
        })
    }

    fn push_gcode(&self, message: &str, kind: &str) {
        let time = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default();
        let mut q = self.gcode.lock().expect("gcode store lock");
        if q.len() == self.gcode_store_size {
            q.pop_front();
        }
        q.push_back(json!({"message": message, "time": time, "type": kind}));
    }

    /// Appends one sample per field, from the latest Klippy status.
    fn sample(&self, app: &App) {
        let mut store = self.temperatures.lock().expect("temperature store lock");
        for (sensor, fields) in store.iter_mut() {
            let current = app.klippy.cached(sensor).unwrap_or_default();
            for (field, samples) in fields.iter_mut() {
                let v = current
                    .get(field.as_str())
                    .map(round2)
                    .unwrap_or_else(|| samples.back().cloned().unwrap_or_default());
                if samples.len() == self.temp_store_size {
                    samples.pop_front();
                }
                samples.push_back(v);
            }
        }
    }
}

/// Rounds to two decimals, keeping null.
fn round2(v: &Value) -> Value {
    match v.as_f64() {
        Some(f) => json!((f * 100.0).round() / 100.0),
        None => Value::Null,
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register("/server/temperature_store", &[RequestType::Get], Transports::All, handler(temperature_store));
    r.register("/server/gcode_store", &[RequestType::Get], Transports::All, handler(gcode_store));
}

pub fn start(app: &Arc<App>) {
    let events = app.events.subscribe();
    tokio::spawn(follow_events(app.clone(), events));
    let sampler = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TEMP_UPDATE_TIME);
        loop {
            tick.tick().await;
            sampler.data_store.sample(&sampler);
        }
    });
}

async fn follow_events(app: Arc<App>, mut rx: broadcast::Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(Event::GcodeResponse(line)) => app.data_store.push_gcode(&line, "response"),
            Ok(Event::GcodeReceived(script)) if !script.trim().is_empty() => {
                app.data_store.push_gcode(&script, "command")
            }
            Ok(Event::KlippyReady) => {
                if let Err(e) = init_sensors(&app).await {
                    tracing::info!("Error configuring sensors: {e}");
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Subscribes to every heater and sensor Klippy lists, and starts their history.
async fn init_sensors(app: &Arc<App>) -> Result<(), crate::api::ApiError> {
    let result = app.klippy.request("objects/query", json!({"objects": {"heaters": null}}), None).await?;
    let heaters = &result["status"]["heaters"];
    let names = |key: &str| -> Vec<String> {
        heaters[key]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let monitors = names("available_monitors");
    let mut sensors = names("available_sensors");
    sensors.extend(monitors.iter().cloned());
    *app.data_store.monitors.lock().expect("monitors lock") = monitors;
    if sensors.is_empty() {
        app.data_store.temperatures.lock().expect("temperature store lock").clear();
        return Ok(());
    }
    let sub: Map<String, Value> = sensors.iter().map(|s| (s.clone(), Value::Null)).collect();
    let status = app.klippy.subscribe_host(app, sub).await?;
    tracing::info!("Configuring available sensors: {sensors:?}");
    let size = app.data_store.temp_store_size;
    let mut store = app.data_store.temperatures.lock().expect("temperature store lock");
    let previous = std::mem::take(&mut *store);
    for sensor in sensors {
        let reported = status["status"][&sensor].as_object().cloned().unwrap_or_default();
        let fields: Vec<&String> = reported.keys().filter(|k| TEMP_FIELDS.contains(&k.as_str())).collect();
        if fields.is_empty() {
            continue;
        }
        let old = previous.iter().find(|(n, _)| *n == sensor).map(|(_, f)| f);
        let entry: SensorStore = fields
            .into_iter()
            .map(|f| {
                let mut samples =
                    old.and_then(|o| o.iter().find(|(n, _)| n == f)).map(|(_, s)| s.clone()).unwrap_or_default();
                if samples.len() == size {
                    samples.pop_front();
                }
                samples.push_back(round2(&reported[f]));
                (f.clone(), samples)
            })
            .collect();
        store.push((sensor, entry));
    }
    Ok(())
}

async fn temperature_store(app: Arc<App>, req: Request) -> ApiResult {
    let include_monitors = req.args.opt_bool("include_monitors")?.unwrap_or(false);
    let monitors = app.data_store.monitors.lock().expect("monitors lock").clone();
    let store = app.data_store.temperatures.lock().expect("temperature store lock");
    let mut out = Map::new();
    for (name, fields) in store.iter() {
        if !include_monitors && monitors.contains(name) {
            continue;
        }
        let f: Map<String, Value> =
            fields.iter().map(|(k, v)| (format!("{k}s"), Value::Array(v.iter().cloned().collect()))).collect();
        out.insert(name.clone(), Value::Object(f));
    }
    Ok(Value::Object(out))
}

async fn gcode_store(app: Arc<App>, req: Request) -> ApiResult {
    let count = req.args.opt_int("count")?;
    let q = app.data_store.gcode.lock().expect("gcode store lock");
    let skip = match count {
        Some(c) if c >= 0 => q.len().saturating_sub(c as usize),
        _ => 0,
    };
    Ok(json!({"gcode_store": q.iter().skip(skip).cloned().collect::<Vec<_>>()}))
}
