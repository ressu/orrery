//! The print job queue, after Moonraker's `job_queue.py`.
//!
//! Jobs wait in order. Starting the queue prints the first job, and with
//! `automatic_transition` the next job starts when the current one completes. A cancelled or
//! failed print pauses the queue. Klippy's macros can pause and start the queue with the
//! `pause_job_queue` and `start_job_queue` remote methods.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::config::Config;
use crate::job_state::JobEvent;
use crate::server::{App, Event};

/// Moonraker sends queue notifications after this delay.
const NOTIFY_DELAY: Duration = Duration::from_millis(50);

struct QueuedJob {
    filename: String,
    job_id: String,
    time_added: f64,
}

impl QueuedJob {
    fn as_dict(&self, now: f64) -> Value {
        json!({
            "filename": self.filename,
            "job_id": self.job_id,
            "time_added": self.time_added,
            "time_in_queue": now - self.time_added,
        })
    }
}

struct State {
    jobs: Vec<QueuedJob>,
    queue_state: &'static str,
    pause_requested: bool,
    /// The pending start of the next job.
    pop: Option<JoinHandle<()>>,
}

pub struct JobQueue {
    /// Held for each queue operation, as Moonraker's `lock` is.
    lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    load_on_start: bool,
    automatic: bool,
    job_delay: f64,
    transition_gcode: String,
    /// Notifications go through one task so they keep their order.
    notify_tx: mpsc::UnboundedSender<(Instant, Value)>,
    notify_rx: Mutex<Option<mpsc::UnboundedReceiver<(Instant, Value)>>>,
    next_id: AtomicU64,
}

impl JobQueue {
    pub fn new(config: &Config) -> Result<Self> {
        let section = config.section("job_queue");
        let load_on_start = section.get_bool("load_on_startup", false)?;
        let automatic = section.get_bool("automatic_transition", false)?;
        let job_delay = section.get_float("job_transition_delay", 0.01)?;
        if job_delay <= 0.0 {
            bail!("Value for option 'job_transition_delay' in section [job_queue] must be above 0.0");
        }
        let transition_gcode = section.get_str("job_transition_gcode", "").trim().to_owned();
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        Ok(Self {
            lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(State { jobs: Vec::new(), queue_state: "paused", pause_requested: false, pop: None }),
            load_on_start,
            automatic,
            job_delay,
            transition_gcode,
            notify_tx,
            notify_rx: Mutex::new(Some(notify_rx)),
            // Job ids look like Moonraker's (Python object ids, in hex).
            next_id: AtomicU64::new(0x7F10_0000_0000 + u64::from(std::process::id()) * 0x1000),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("job queue lock")
    }

    fn queue_state(&self) -> &'static str {
        self.state().queue_state
    }

    fn set_queue_state(&self, new_state: &'static str) {
        let mut st = self.state();
        if st.queue_state != new_state {
            st.queue_state = new_state;
            self.send_queue_event(&st, "state_changed");
        }
    }

    fn send_queue_event(&self, st: &State, action: &str) {
        let updated_queue = if action == "state_changed" { Value::Null } else { job_list(st) };
        let params = json!({"action": action, "updated_queue": updated_queue, "queue_state": st.queue_state});
        let _ = self.notify_tx.send((Instant::now() + NOTIFY_DELAY, params));
    }

    fn status(&self) -> Value {
        let st = self.state();
        json!({"queued_jobs": job_list(&st), "queue_state": st.queue_state})
    }
}

fn job_list(st: &State) -> Value {
    let now = now();
    Value::Array(st.jobs.iter().map(|j| j.as_dict(now)).collect())
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default()
}

pub fn register_endpoints(app: &Arc<App>) {
    use RequestType::{Delete, Get, Post};
    let mut r = app.registry.write().expect("registry lock");
    r.register("/server/job_queue/job", &[Post, Delete], Transports::All, handler(job_request));
    r.register("/server/job_queue/pause", &[Post], Transports::All, handler(|app, _| pause_request(app)));
    r.register("/server/job_queue/start", &[Post], Transports::All, handler(|app, _| start_request(app)));
    r.register("/server/job_queue/status", &[Get], Transports::All, handler(|app, _| status_request(app)));
    r.register("/server/job_queue/jump", &[Post], Transports::All, handler(jump));
}

pub fn start(app: &Arc<App>) {
    if let Some(mut rx) = app.job_queue.notify_rx.lock().expect("job queue lock").take() {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some((at, params)) = rx.recv().await {
                tokio::time::sleep_until(at).await;
                app.notify("job_queue_changed", vec![params]);
            }
        });
    }
    tokio::spawn(follow_events(app.clone(), app.events.subscribe()));
}

async fn follow_events(app: Arc<App>, mut rx: broadcast::Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(Event::KlippyReady) => on_ready(&app).await,
            Ok(Event::KlippyShutdown) => {
                let requested = app.job_queue.state().pause_requested;
                pause_queue(app.clone()).await;
                app.job_queue.state().pause_requested = requested;
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn on_ready(app: &Arc<App>) {
    let q = &app.job_queue;
    let _guard = q.lock.lock().await;
    if !q.load_on_start || q.state().jobs.is_empty() {
        return;
    }
    if matches!(q.queue_state(), "ready" | "paused") {
        q.set_queue_state("loading");
        schedule_pop(app, 1.0, false);
    }
}

/// Moonraker's `_on_job_state_changed`.
pub async fn on_job_event(app: Arc<App>, event: JobEvent) {
    let q = &app.job_queue;
    if event == JobEvent::Complete {
        if !q.automatic {
            return;
        }
        let _guard = q.lock.lock().await;
        if q.queue_state() == "ready" && !q.state().jobs.is_empty() {
            q.set_queue_state("loading");
            schedule_pop(&app, q.job_delay, true);
        }
    } else if event.aborted() {
        let _guard = q.lock.lock().await;
        if !q.state().jobs.is_empty() {
            q.set_queue_state("paused");
        }
    }
}

fn schedule_pop(app: &Arc<App>, delay: f64, need_transition: bool) {
    let task_app = app.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        pop_job(&task_app, need_transition).await;
    });
    app.job_queue.state().pop = Some(handle);
}

/// Starts the first job in the queue.
async fn pop_job(app: &Arc<App>, need_transition: bool) {
    let q = &app.job_queue;
    q.state().pop = None;
    let _guard = q.lock.lock().await;
    if q.queue_state() == "paused" {
        return;
    }
    let first = q.state().jobs.first().map(|j| (j.job_id.clone(), j.filename.clone()));
    let Some((uid, filename)) = first else {
        q.set_queue_state("paused");
        return;
    };
    if !check_can_print(app).await || q.queue_state() != "loading" {
        q.set_queue_state("paused");
        return;
    }
    if need_transition && !q.transition_gcode.is_empty() {
        if let Err(e) = crate::klippy::run_gcode(app, &q.transition_gcode).await {
            tracing::warn!("Error Loading print: {filename}: {e}");
            q.set_queue_state("paused");
            return;
        }
        // The queue may have been paused while the transition gcode ran.
        if q.queue_state() != "loading" {
            q.set_queue_state("paused");
            tracing::warn!("Error Loading print: {filename}: Queue State Changed during Transition Gcode");
            return;
        }
    }
    q.set_queue_state("starting");
    app.klippy.wait_started().await;
    if let Err(e) = crate::klippy::start_print(app, &filename).await {
        tracing::warn!("Error Loading print: {filename}: {e}");
        q.set_queue_state("paused");
        return;
    }
    let mut st = q.state();
    st.jobs.retain(|j| j.job_id != uid);
    if st.queue_state == "starting" {
        st.queue_state = if !st.jobs.is_empty() && q.automatic { "ready" } else { "paused" };
    }
    q.send_queue_event(&st, "job_loaded");
}

async fn check_can_print(app: &Arc<App>) -> bool {
    let params = json!({"objects": {"print_stats": null}});
    let Ok(result) = app.klippy.request("objects/query", params, None).await else { return false };
    match result["status"].get("print_stats") {
        Some(ps) => !matches!(ps["state"].as_str(), Some("printing" | "paused")),
        None => false,
    }
}

/// Adds files to the end of the queue. With `reset`, the queue is cleared first.
pub async fn queue_job(
    app: &Arc<App>,
    filenames: Vec<String>,
    check_exists: bool,
    reset: bool,
) -> Result<(), ApiError> {
    let q = &app.job_queue;
    let _guard = q.lock.lock().await;
    if check_exists {
        let gcodes = app.files.gcodes_dir();
        for name in &filenames {
            let path = crate::files::normalize(&gcodes.join(name));
            if !path.starts_with(gcodes) || !path.is_file() {
                return Err(ApiError::bad_request(format!("G-Code File {name} does not exist")));
            }
        }
    }
    {
        let mut st = q.state();
        if reset {
            st.jobs.clear();
        }
        let time_added = now();
        for filename in filenames {
            let job_id = format!("{:016X}", q.next_id.fetch_add(0x60, Ordering::Relaxed));
            st.jobs.push(QueuedJob { filename, job_id, time_added });
        }
        q.send_queue_event(&st, "jobs_added");
    }
    if q.automatic && !q.state().pause_requested {
        let last = app.job_state.last_event();
        if last.is_printing() || last == JobEvent::Paused {
            q.set_queue_state("ready");
        }
    }
    Ok(())
}

async fn delete_jobs(app: &Arc<App>, job_ids: &[String], all: bool) {
    let q = &app.job_queue;
    let _guard = q.lock.lock().await;
    let mut st = q.state();
    if st.jobs.is_empty() {
        return;
    }
    if all {
        st.jobs.clear();
    } else if !job_ids.is_empty() {
        st.jobs.retain(|j| !job_ids.contains(&j.job_id));
    } else {
        // Nothing was deleted, so there's nothing to tell clients.
        return;
    }
    q.send_queue_event(&st, "jobs_removed");
}

pub async fn pause_queue(app: Arc<App>) {
    let q = &app.job_queue;
    q.set_queue_state("paused");
    if let Some(handle) = q.state().pop.take() {
        handle.abort();
    }
    // Wait for any operation in progress to finish.
    let _guard = q.lock.lock().await;
    q.state().pause_requested = true;
}

pub async fn start_queue(app: Arc<App>) {
    let q = &app.job_queue;
    let _guard = q.lock.lock().await;
    q.state().pause_requested = false;
    if !matches!(q.queue_state(), "ready" | "paused") {
        return;
    }
    if q.state().jobs.is_empty() {
        q.set_queue_state("paused");
    } else if check_can_print(&app).await {
        q.set_queue_state("loading");
        schedule_pop(&app, 0.01, false);
    } else {
        q.set_queue_state(if q.automatic { "ready" } else { "paused" });
    }
}

async fn job_request(app: Arc<App>, req: Request) -> ApiResult {
    match req.kind {
        RequestType::Post => {
            let files = req.args.list("filenames")?;
            let reset = req.args.opt_bool("reset")?.unwrap_or(false);
            queue_job(&app, files, true, reset).await?;
        }
        RequestType::Delete => {
            if req.args.opt_bool("all")?.unwrap_or(false) {
                delete_jobs(&app, &[], true).await;
            } else {
                let job_ids = req.args.list("job_ids")?;
                delete_jobs(&app, &job_ids, false).await;
            }
        }
        RequestType::Get => return Err(ApiError::bad_request("Invalid request type: GET")),
    }
    Ok(app.job_queue.status())
}

async fn pause_request(app: Arc<App>) -> ApiResult {
    pause_queue(app.clone()).await;
    Ok(app.job_queue.status())
}

async fn start_request(app: Arc<App>) -> ApiResult {
    start_queue(app.clone()).await;
    Ok(app.job_queue.status())
}

async fn status_request(app: Arc<App>) -> ApiResult {
    Ok(app.job_queue.status())
}

async fn jump(app: Arc<App>, req: Request) -> ApiResult {
    let job_id = req.args.str("job_id")?;
    let q = &app.job_queue;
    let _guard = q.lock.lock().await;
    let mut st = q.state();
    let Some(pos) = st.jobs.iter().position(|j| j.job_id == job_id) else {
        return Err(ApiError::bad_request(format!("Invalid job id: {job_id}")));
    };
    let job = st.jobs.remove(pos);
    st.jobs.insert(0, job);
    drop(st);
    Ok(q.status())
}

/// Stops a pending job start when orrery exits.
pub async fn on_exit(app: &Arc<App>) {
    pause_queue(app.clone()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_delay() {
        let c = Config::parse("[job_queue]\njob_transition_delay: 0\n").unwrap();
        assert!(JobQueue::new(&c).is_err());
        let c = Config::parse("[job_queue]\nautomatic_transition: true\n").unwrap();
        let q = JobQueue::new(&c).unwrap();
        assert!(q.automatic);
        assert_eq!(q.job_delay, 0.01);
    }
}
