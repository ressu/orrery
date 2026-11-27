//! Print history and totals, after Moonraker's `history.py`. Jobs are added and finished from
//! the job events [`crate::job_state`] sends, and each change is announced with
//! `notify_history_changed`.
//!
//! Moonraker's auxiliary fields come from its sensor and spoolman components. In orrery, agents
//! set them instead, with `orrery.agent.history_field`, while a job is recorded. Agent fields
//! have no totals, so `auxiliary_totals` stays empty.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::database::TotalsRow;
use crate::job_state::JobEvent;
use crate::server::App;

const NO_USER: &str = "No User";

/// Moonraker's `BASE_TOTALS`, in its order.
fn base_totals() -> Map<String, Value> {
    let mut t = Map::new();
    t.insert("total_jobs".into(), json!(0));
    for k in ["total_time", "total_print_time", "total_filament_used", "longest_job", "longest_print"] {
        t.insert(k.into(), json!(0.0));
    }
    t
}

pub struct History {
    /// Moonraker's `request_lock`: requests and job changes take turns.
    state: tokio::sync::Mutex<Current>,
    totals: Mutex<Map<String, Value>>,
}

struct Current {
    job: Option<PrinterJob>,
    id: Option<u64>,
    /// Who asked for the print, from [`job_requested`].
    user: String,
}

impl Default for History {
    fn default() -> Self {
        Self {
            state: tokio::sync::Mutex::new(Current { job: None, id: None, user: NO_USER.into() }),
            totals: Mutex::new(base_totals()),
        }
    }
}

impl History {
    fn totals(&self) -> Map<String, Value> {
        self.totals.lock().expect("totals lock").clone()
    }
}

/// Moonraker's `PrinterJob`.
#[derive(Debug, Clone)]
struct PrinterJob {
    end_time: Option<f64>,
    filament_used: f64,
    filename: String,
    metadata: Map<String, Value>,
    print_duration: f64,
    status: String,
    start_time: f64,
    total_duration: f64,
    user: String,
    /// Fields agents set while the job runs.
    auxiliary_data: Vec<Value>,
}

impl PrinterJob {
    fn new(ps: &Map<String, Value>) -> Self {
        let mut job = Self {
            end_time: None,
            filament_used: 0.0,
            filename: String::new(),
            metadata: Map::new(),
            print_duration: 0.0,
            status: "in_progress".into(),
            start_time: now(),
            total_duration: 0.0,
            user: NO_USER.into(),
            auxiliary_data: Vec::new(),
        };
        job.update_from_ps(ps);
        job
    }

    /// Takes the `print_stats` fields that match a job field.
    fn update_from_ps(&mut self, ps: &Map<String, Value>) {
        if let Some(f) = ps.get("filename").and_then(Value::as_str) {
            self.filename = f.to_owned();
        }
        for (key, field) in [
            ("filament_used", &mut self.filament_used),
            ("print_duration", &mut self.print_duration),
            ("total_duration", &mut self.total_duration),
        ] {
            if let Some(v) = ps.get(key).and_then(Value::as_f64) {
                *field = v;
            }
        }
    }

    fn finish(&mut self, status: &str, ps: &Map<String, Value>) {
        self.end_time = Some(now());
        self.status = status.to_owned();
        self.update_from_ps(ps);
    }

    /// The job as Moonraker's notification shows it, in `PrinterJob.__dict__` order.
    fn stats(&self) -> Map<String, Value> {
        let v = json!({
            "end_time": self.end_time,
            "filament_used": self.filament_used,
            "filename": self.filename,
            "metadata": self.metadata,
            "print_duration": self.print_duration,
            "status": self.status,
            "start_time": self.start_time,
            "total_duration": self.total_duration,
            "auxiliary_data": self.auxiliary_data,
            "user": self.user,
        });
        v.as_object().cloned().unwrap_or_default()
    }

    /// The stored row: the `job_history` columns after `job_id`, without `instance_id`.
    fn row(&self) -> Value {
        json!({
            "user": self.user,
            "filename": self.filename,
            "status": self.status,
            "start_time": self.start_time,
            "end_time": self.end_time,
            "print_duration": self.print_duration,
            "total_duration": self.total_duration,
            "filament_used": self.filament_used,
            "metadata": self.metadata,
            "auxiliary_data": self.auxiliary_data,
        })
    }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

fn job_uid(id: u64) -> String {
    format!("{id:06X}")
}

fn parse_uid(uid: &str) -> Result<u64, ApiError> {
    u64::from_str_radix(uid.trim(), 16).map_err(|_| ApiError::bad_request(format!("Invalid job uid: {uid}")))
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register("/server/history/job", &[RequestType::Get, RequestType::Delete], Transports::All, handler(job_request));
    r.register("/server/history/list", &[RequestType::Get], Transports::All, handler(jobs_list));
    r.register("/server/history/totals", &[RequestType::Get], Transports::All, handler(job_totals));
    r.register("/server/history/reset_totals", &[RequestType::Post], Transports::All, handler(reset_totals));
}

/// Loads the totals and marks jobs left in progress by an earlier run as interrupted.
pub fn start(app: &Arc<App>) {
    match app.database.totals_load() {
        Ok(rows) => {
            let mut totals = app.history.totals.lock().expect("totals lock");
            for (provider, field, maximum, total) in rows {
                if provider == "history" {
                    totals.insert(field, if maximum.is_null() { total } else { maximum });
                }
            }
        }
        Err(e) => tracing::warn!("Can't load history totals: {e:#}"),
    }
    match app.database.history_replace_status("in_progress", "interrupted") {
        Ok(ids) if !ids.is_empty() => {
            let ids: Vec<u64> = ids;
            tracing::info!("The following jobs were detected as interrupted: {ids:?}");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("Can't check for interrupted jobs: {e:#}"),
    }
}

/// Moonraker's `_on_job_state_changed`.
pub async fn on_job_event(app: &Arc<App>, event: JobEvent, prev: &Map<String, Value>, new: &Map<String, Value>) {
    match event {
        JobEvent::Started => {
            if app.history.state.lock().await.job.is_some() {
                finish_job(app, "cancelled", Some(prev)).await;
            }
            add_job(app, PrinterJob::new(new)).await;
        }
        JobEvent::Complete => finish_job(app, "completed", Some(new)).await,
        JobEvent::Error => finish_job(app, "error", Some(new)).await,
        // Standby counts as cancelled, for the `CLEAR_PAUSE`/`SDCARD_RESET_FILE` workflow.
        JobEvent::Cancelled | JobEvent::Standby => finish_job(app, "cancelled", Some(prev)).await,
        JobEvent::Paused | JobEvent::Resumed => {}
    }
}

/// Records who started the print, after Moonraker's `klippy_apis:job_start_complete`.
pub async fn job_requested(app: &Arc<App>, user: &str) {
    let mut cur = app.history.state.lock().await;
    cur.user = user.to_owned();
    if let Some(job) = &mut cur.job {
        job.user = user.to_owned();
    }
}

async fn add_job(app: &Arc<App>, mut job: PrinterJob) {
    let mut cur = app.history.state.lock().await;
    job.user = cur.user.clone();
    job.metadata = job_metadata(app, &job.filename);
    let saved = app.database.history_next_id().and_then(|id| app.database.history_put(id, &job.row()).map(|()| id));
    cur.job = Some(job);
    cur.id = None;
    let id = match saved {
        Ok(id) => id,
        Err(e) => {
            tracing::info!(
                "Error saving job, filename '{}': {e:#}",
                cur.job.as_ref().map(|j| j.filename.as_str()).unwrap_or("")
            );
            return;
        }
    };
    cur.id = Some(id);
    let job = cur.job.as_ref().expect("job was just set");
    app.files.set_job_info(&app.database, &job.filename, job.start_time, &job_uid(id));
    tracing::debug!("History Job Added - Id: {}, File: {}", job_uid(id), job.filename);
    send_history_event(app, "added", job, id);
}

/// Finishes the current job with `status`. `stats` defaults to the last `print_stats`.
pub async fn finish_job(app: &Arc<App>, status: &str, stats: Option<&Map<String, Value>>) {
    let mut cur = app.history.state.lock().await;
    let (Some(mut job), Some(id)) = (cur.job.take(), cur.id.take()) else {
        reset(&mut cur);
        return;
    };
    let mut ps = stats.cloned().unwrap_or_else(|| app.job_state.last_stats());
    let duration = ps.get("total_duration").and_then(Value::as_f64).unwrap_or(0.0);
    if ps.get("filename").and_then(Value::as_str) != Some(job.filename.as_str()) || duration < job.total_duration {
        // The print stats were reset, so they don't belong to this job.
        ps = Map::new();
    }
    job.user = cur.user.clone();
    job.finish(status, &ps);
    // Read the metadata again, in case an upload's metadata wasn't ready when the job started.
    job.metadata = job_metadata(app, &job.filename);
    if let Err(e) = app.database.history_put(id, &job.row()) {
        tracing::warn!("Can't save job {}: {e:#}", job_uid(id));
    }
    app.files.set_job_info(&app.database, &job.filename, job.start_time, &job_uid(id));
    update_totals(app, &job);
    tracing::debug!("History Job Finished - Id: {}, File: {}, Status: {status}", job_uid(id), job.filename);
    send_history_event(app, "finished", &job, id);
    reset(&mut cur);
}

/// Sets an agent's field on the job being recorded, as Moonraker's `auxiliary_data` entry. The
/// field is saved with the job.
pub async fn set_field(
    app: &Arc<App>,
    provider: &str,
    name: &str,
    value: Value,
    description: Option<String>,
    units: Option<String>,
) -> Result<(), ApiError> {
    let mut cur = app.history.state.lock().await;
    let Some(job) = cur.job.as_mut() else {
        return Err(ApiError::bad_request("No print job is being recorded"));
    };
    let field = json!({"provider": provider, "name": name, "value": value, "description": description, "units": units});
    match job.auxiliary_data.iter_mut().find(|f| f["provider"] == provider && f["name"] == name) {
        Some(existing) => *existing = field,
        None => job.auxiliary_data.push(field),
    }
    Ok(())
}

/// Finishes a job still in progress when orrery stops.
pub async fn on_exit(app: &Arc<App>) {
    if app.history.state.lock().await.job.is_some() {
        finish_job(app, "server_exit", None).await;
    }
}

fn reset(cur: &mut Current) {
    cur.job = None;
    cur.id = None;
    cur.user = NO_USER.into();
}

/// The file's metadata without the fields history keeps elsewhere.
fn job_metadata(app: &Arc<App>, filename: &str) -> Map<String, Value> {
    let mut md = app.files.get_metadata(filename).unwrap_or_default();
    md.remove("print_start_time");
    md.remove("job_id");
    if let Some(Value::Array(thumbs)) = md.get_mut("thumbnails") {
        for t in thumbs.iter_mut().filter_map(Value::as_object_mut) {
            t.remove("data");
        }
    }
    md
}

fn update_totals(app: &Arc<App>, job: &PrinterJob) {
    let rows = {
        let mut t = app.history.totals.lock().expect("totals lock");
        let get = |t: &Map<String, Value>, k: &str| t.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let jobs = t.get("total_jobs").and_then(Value::as_i64).unwrap_or(0) + 1;
        t.insert("total_jobs".into(), json!(jobs));
        for (k, add) in [
            ("total_time", job.total_duration),
            ("total_print_time", job.print_duration),
            ("total_filament_used", job.filament_used),
        ] {
            let v = get(&t, k) + add;
            t.insert(k.into(), json!(v));
        }
        for (k, v) in [("longest_job", job.total_duration), ("longest_print", job.print_duration)] {
            let m = get(&t, k).max(v);
            t.insert(k.into(), json!(m));
        }
        totals_rows(&t)
    };
    if let Err(e) = app.database.totals_save(&rows) {
        tracing::warn!("Can't save history totals: {e:#}");
    }
}

/// Moonraker's `_create_totals_list`: `total_*` fields are totals, the rest are maximums.
fn totals_rows(totals: &Map<String, Value>) -> Vec<TotalsRow> {
    totals
        .iter()
        .map(|(k, v)| {
            let (maximum, total) =
                if k.starts_with("total_") { (Value::Null, v.clone()) } else { (v.clone(), Value::Null) };
            ("history".to_owned(), k.clone(), maximum, total)
        })
        .collect()
}

fn send_history_event(app: &Arc<App>, action: &str, job: &PrinterJob, id: u64) {
    let job = prep_job(app, job.stats(), id);
    let filename = job.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned();
    let file_info = app.files.file_info_map([filename.as_str()]);
    app.notify("history_changed", vec![json!({"action": action, "job": job, "file_info": file_info})]);
}

/// Moonraker's `_prep_requested_job`: adds `exists` and the hex `job_id`.
fn prep_job(app: &Arc<App>, mut job: Map<String, Value>, id: u64) -> Map<String, Value> {
    let filename = job.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned();
    let modified = job.get("metadata").and_then(|m| m.get("modified")).and_then(Value::as_f64);
    job.insert("exists".into(), json!(file_exists(app, &filename, modified)));
    job.insert("job_id".into(), json!(job_uid(id)));
    job
}

/// A stored row with `job_id` first, as `dict(row)` gives it.
fn row_job(app: &Arc<App>, id: u64, row: Value) -> Map<String, Value> {
    let mut job = Map::new();
    job.insert("job_id".into(), json!(job_uid(id)));
    if let Value::Object(fields) = row {
        job.extend(fields);
    }
    prep_job(app, job, id)
}

/// Moonraker's `check_file_exists`: the file is there and, if known, has the same modified time.
fn file_exists(app: &Arc<App>, filename: &str, modified: Option<f64>) -> bool {
    let path = app.files.gcodes_dir().join(filename);
    let Ok(meta) = std::fs::metadata(&path) else { return false };
    if !meta.is_file() {
        return false;
    }
    let Some(expected) = modified else { return true };
    let mtime = meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs_f64());
    // math.isclose with its default relative tolerance.
    mtime.is_some_and(|m| (m - expected).abs() <= 1e-9 * m.abs().max(expected.abs()))
}

fn db_error(e: anyhow::Error) -> ApiError {
    ApiError::new(500, format!("{e:#}"))
}

async fn job_request(app: Arc<App>, req: Request) -> ApiResult {
    let _guard = app.history.state.lock().await;
    if req.kind == RequestType::Get {
        let uid = req.args.str("uid")?;
        let id = parse_uid(&uid)?;
        let row = app.database.history_get(id).map_err(db_error)?;
        let row = row.ok_or_else(|| ApiError::not_found(format!("Invalid job uid: {uid}")))?;
        let job = row_job(&app, id, row);
        let filename = job.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned();
        let file_info = app.files.file_info_map([filename.as_str()]);
        return Ok(json!({"job": job, "file_info": file_info}));
    }
    if req.args.opt_bool("all")?.unwrap_or(false) {
        let ids = app.database.history_delete_all().map_err(db_error)?;
        return Ok(json!({"deleted_jobs": ids.into_iter().map(job_uid).collect::<Vec<_>>()}));
    }
    let uid = req.args.str("uid")?;
    if !app.database.history_delete(parse_uid(&uid)?).map_err(db_error)? {
        return Err(ApiError::not_found(format!("Invalid job uid: {uid}")));
    }
    Ok(json!({"deleted_jobs": [uid]}))
}

async fn jobs_list(app: Arc<App>, req: Request) -> ApiResult {
    let _guard = app.history.state.lock().await;
    let before = req.args.opt_float("before")?.unwrap_or(-1.0);
    let since = req.args.opt_float("since")?.unwrap_or(-1.0);
    let limit = req.args.opt_int("limit")?.unwrap_or(50);
    let start = req.args.opt_int("start")?.unwrap_or(0);
    let order = req.args.opt_str("order")?.unwrap_or_else(|| "desc".into()).to_uppercase();
    if order != "ASC" && order != "DESC" {
        return Err(ApiError::bad_request(format!("Invalid `order` value: {order}")));
    }
    let mut skip = if limit > 0 { start.max(0) } else { 0 };
    let mut jobs = Vec::new();
    let mut matched = Vec::new();
    app.database
        .history_scan(order == "DESC", |id, row| {
            let num = |k: &str| row.get(k).and_then(Value::as_f64);
            // SQL comparisons with a NULL end_time are false.
            if before != -1.0 && !num("end_time").is_some_and(|e| e < before) {
                return true;
            }
            if since != -1.0 && !num("start_time").is_some_and(|s| s > since) {
                return true;
            }
            if skip > 0 {
                skip -= 1;
                return true;
            }
            matched.push((id, row));
            limit <= 0 || (matched.len() as i64) < limit
        })
        .map_err(db_error)?;
    for (id, row) in matched {
        jobs.push(Value::Object(row_job(&app, id, row)));
    }
    let mut result = json!({"count": jobs.len(), "jobs": jobs});
    app.files.add_file_info(&mut result, "jobs");
    Ok(result)
}

async fn job_totals(app: Arc<App>, _req: Request) -> ApiResult {
    Ok(json!({"job_totals": app.history.totals(), "auxiliary_totals": []}))
}

async fn reset_totals(app: Arc<App>, _req: Request) -> ApiResult {
    if app.history.state.lock().await.job.is_some() {
        return Err(ApiError::bad_request("Job in progress, cannot reset totals"));
    }
    let last = std::mem::replace(&mut *app.history.totals.lock().expect("totals lock"), base_totals());
    app.database.totals_save(&totals_rows(&base_totals())).map_err(db_error)?;
    Ok(json!({"last_totals": last, "last_auxiliary_totals": []}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_from_print_stats() {
        let ps = json!({"filename": "a.gcode", "state": "printing", "total_duration": 5.0, "info": {}});
        let mut job = PrinterJob::new(ps.as_object().unwrap());
        assert_eq!((job.filename.as_str(), job.total_duration, job.status.as_str()), ("a.gcode", 5.0, "in_progress"));
        job.finish("completed", json!({"print_duration": 3.5}).as_object().unwrap());
        assert_eq!((job.status.as_str(), job.print_duration), ("completed", 3.5));
        assert!(job.end_time.is_some());
    }

    #[test]
    fn totals_split_into_maximum_and_total() {
        let rows = totals_rows(&base_totals());
        assert_eq!(rows[0], ("history".into(), "total_jobs".into(), Value::Null, json!(0)));
        assert_eq!(rows[4], ("history".into(), "longest_job".into(), json!(0.0), Value::Null));
        assert_eq!(job_uid(10), "00000A");
        assert_eq!(parse_uid("00000A").unwrap(), 10);
    }
}
