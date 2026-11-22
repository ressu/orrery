//! The update manager: its entries, the state it keeps in orrery's database, the checks it
//! runs on a schedule, and the print state that holds them back
//! (docs/design/2026-11-22-update-manager.md).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use orrery_agent::{Agent, MethodError};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

use crate::config::{self, Entry, Global, Kind, SECTION};
use crate::git::{self, RepoState, Tracking};

/// Moonraker skips a refresh of everything within this long of the last one.
const REFRESH_SPAM: Duration = Duration::from_secs(60);

/// One entry and what's known about it.
struct EntryState {
    entry: Entry,
    stored: Stored,
    /// The last refresh's findings, since this agent started.
    repo: Option<RepoState>,
}

/// What the database keeps for each entry, under its name in the `update_manager` namespace.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stored {
    /// Seconds since 1970.
    pub last_refresh_time: f64,
    pub config_hash: String,
    /// The entry's object in `version_info` after the last refresh.
    pub status: Value,
    pub tracking: Tracking,
    pub rollback_version: Option<String>,
}

impl Stored {
    pub fn from_json(v: &Value) -> Self {
        let text = |k: &str| v[k].as_str().map(str::to_owned);
        Self {
            last_refresh_time: v["last_refresh_time"].as_f64().unwrap_or(0.0),
            config_hash: text("config_hash").unwrap_or_default(),
            status: v.get("status").cloned().unwrap_or(Value::Null),
            tracking: Tracking { branch: text("branch"), remote: text("remote") },
            rollback_version: text("rollback_version"),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "last_refresh_time": self.last_refresh_time,
            "config_hash": self.config_hash,
            "status": self.status,
            "branch": self.tracking.branch,
            "remote": self.tracking.remote,
            "rollback_version": self.rollback_version,
        })
    }
}

/// Whether an entry is due for a check: its interval has passed, or its config changed.
pub fn is_due(entry: &Entry, stored: &Stored, now: f64) -> bool {
    stored.config_hash != entry.hash || now >= stored.last_refresh_time + entry.refresh_interval.as_secs_f64()
}

/// When the next check is due, in seconds since 1970.
pub fn next_due(entries: &[(&Entry, &Stored)]) -> Option<f64> {
    entries.iter().map(|(e, s)| s.last_refresh_time + e.refresh_interval.as_secs_f64()).reduce(f64::min)
}

/// The stored entries that aren't in the config any more.
fn stale_keys<'a>(stored: &'a Map<String, Value>, entries: &[Entry]) -> Vec<&'a String> {
    stored.keys().filter(|k| !entries.iter().any(|e| &e.name == *k)).collect()
}

/// How long until `hour` is inside the window `(start, end)`, in seconds, from `now` given as
/// the local `(hour, minute, second)`. Zero inside the window.
pub fn until_window((start, end): (u32, u32), (hour, minute, second): (u32, u32, u32)) -> u64 {
    let inside = if start < end { start <= hour && hour < end } else { hour >= start || hour < end };
    if inside {
        return 0;
    }
    let now = u64::from(hour * 3600 + minute * 60 + second);
    let open = u64::from(start * 3600);
    if open > now { open - now } else { open + 86400 - now }
}

pub struct Manager {
    agent: Mutex<Option<Agent>>,
    global: Mutex<Option<Global>>,
    /// Held while a command runs, and only then: one command at a time.
    entries: tokio::sync::Mutex<Vec<EntryState>>,
    /// Each entry's object in `version_info`, readable while a command runs.
    view: Mutex<Vec<(String, Value)>>,
    busy: Mutex<bool>,
    /// `print_stats.state`, or empty while Klippy isn't there.
    print_state: watch::Sender<String>,
    /// Wakes the schedule, when the config or the print state changes.
    wake: Notify,
    last_refresh_all: Mutex<Option<Instant>>,
    /// Config problems, for `server.info`.
    warnings: Mutex<Vec<String>>,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn busy_error() -> MethodError {
    MethodError::new(503, "Server is busy, cannot perform refresh")
}

impl Manager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agent: Mutex::new(None),
            global: Mutex::new(None),
            entries: tokio::sync::Mutex::new(Vec::new()),
            view: Mutex::new(Vec::new()),
            busy: Mutex::new(false),
            print_state: watch::channel(String::new()).0,
            wake: Notify::new(),
            last_refresh_all: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
        })
    }

    fn agent(&self) -> Option<Agent> {
        self.agent.lock().expect("agent lock").clone()
    }

    /// Printing or paused: no checks or updates then.
    pub fn is_printing(&self) -> bool {
        matches!(self.print_state.borrow().as_str(), "printing" | "paused")
    }

    fn is_busy(&self) -> bool {
        *self.busy.lock().expect("busy lock")
    }

    /// Runs each time the agent connects. The first time, it reads the config and the stored
    /// state, and starts the schedule.
    pub async fn setup(self: &Arc<Self>, agent: Agent) -> Result<(), orrery_agent::Error> {
        *self.agent.lock().expect("agent lock") = Some(agent.clone());
        let first = self.global.lock().expect("global lock").is_none();
        if first {
            self.load(&agent).await?;
            let m = self.clone();
            tokio::spawn(async move { m.schedule().await });
        } else {
            // A new connection has no warnings yet.
            let warnings = self.warnings.lock().expect("warnings lock").clone();
            agent.set_warnings(&warnings).await?;
        }
        agent.register_endpoint("status", &["GET"]).await?;
        agent.register_endpoint("refresh", &["POST"]).await?;
        agent.register_endpoint("klipper", &["POST"]).await?;
        agent.register_endpoint("moonraker", &["POST"]).await?;

        let m = self.clone();
        let mut notifications = agent.notifications();
        let a = agent.clone();
        tokio::spawn(async move {
            loop {
                let n = tokio::select! {
                    n = notifications.recv() => n,
                    () = a.closed() => break,
                };
                let n = match n {
                    Ok(n) => n,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                match n.method.as_str() {
                    "notify_status_update" => {
                        if let Some(state) = n.params[0]["print_stats"]["state"].as_str() {
                            m.set_print_state(state);
                        }
                    }
                    "notify_klippy_ready" => {
                        let m = m.clone();
                        tokio::spawn(async move { m.subscribe().await });
                    }
                    "notify_klippy_disconnected" | "notify_klippy_shutdown" => m.set_print_state(""),
                    _ => {}
                }
            }
            // Without orrery, nothing says when a print starts.
            m.set_print_state("");
        });
        let info = agent.call("server.info", Value::Null).await?;
        if info["klippy_state"] == "ready" {
            self.subscribe().await;
        }
        Ok(())
    }

    fn set_print_state(&self, state: &str) {
        let changed = self.print_state.send_if_modified(|s| {
            if s == state {
                return false;
            }
            state.clone_into(s);
            true
        });
        if changed {
            self.wake.notify_one();
        }
    }

    async fn subscribe(&self) {
        let Some(agent) = self.agent() else { return };
        match agent.call("printer.objects.subscribe", json!({"objects": {"print_stats": ["state"]}})).await {
            Ok(reply) => {
                let state = reply["status"]["print_stats"]["state"].as_str().unwrap_or_default().to_owned();
                self.set_print_state(&state);
            }
            Err(e) => tracing::warn!("Can't follow the print state, so updates are allowed: {e}"),
        }
    }

    /// Reads the config, and each entry's stored state.
    async fn load(&self, agent: &Agent) -> Result<(), orrery_agent::Error> {
        let mut warnings = Vec::new();
        let global_map = agent.config(None).await?.unwrap_or_default();
        let global = config::global(&global_map, &mut warnings);
        let mut entries = Vec::new();
        for name in agent.config_names(None).await? {
            let map = agent.config(Some(&format!("{SECTION} {name}"))).await?.unwrap_or_default();
            match config::entry(&name, &map, &global) {
                Ok(e) => entries.push(e),
                Err(w) => warnings.push(w),
            }
        }
        // The reply is `{namespace, key, value}`, with the whole namespace as the value. A
        // namespace that isn't there yet is an error.
        let stored = match agent.call("server.database.get_item", json!({"namespace": SECTION})).await {
            Ok(reply) => reply["value"].as_object().cloned().unwrap_or_default(),
            Err(_) => Map::new(),
        };
        // Entries that are gone from the config leave no state behind.
        for key in stale_keys(&stored, &entries) {
            let _ = agent.call("server.database.delete_item", json!({"namespace": SECTION, "key": key})).await;
        }
        let states: Vec<EntryState> = entries
            .into_iter()
            .map(|entry| {
                let stored = stored.get(&entry.name).map(Stored::from_json).unwrap_or_default();
                EntryState { entry, stored, repo: None }
            })
            .collect();
        tracing::info!(
            "{} update manager entries, checked every {} hours{}",
            states.len(),
            global.refresh_interval.as_secs() / 3600,
            if global.enable_auto_refresh { "" } else { " only at startup" }
        );
        *self.view.lock().expect("view lock") = states.iter().map(|s| (s.entry.name.clone(), view_of(s))).collect();
        *self.entries.lock().await = states;
        *self.global.lock().expect("global lock") = Some(global);
        agent.set_warnings(&warnings).await?;
        *self.warnings.lock().expect("warnings lock") = warnings;
        Ok(())
    }

    /// `machine.update.status`: every entry's state. With `refresh`, everything is checked
    /// first, unless a print or a command is running, or the last check was a minute ago;
    /// then the request is answered without a check, as Moonraker does.
    pub async fn status(self: &Arc<Self>, params: &Value) -> Result<Value, MethodError> {
        let refresh = match &params["refresh"] {
            Value::Bool(b) => *b,
            Value::String(s) => s.eq_ignore_ascii_case("true"),
            _ => false,
        };
        let recent = self.last_refresh_all.lock().expect("refresh lock").is_some_and(|t| t.elapsed() < REFRESH_SPAM);
        if refresh && !recent && !self.is_printing() && !self.is_busy() {
            self.refresh_entries(None).await;
            self.notify_refreshed().await;
        }
        Ok(self.status_json())
    }

    /// `machine.update.refresh`: checks one entry, or all of them.
    pub async fn refresh(self: &Arc<Self>, params: &Value) -> Result<Value, MethodError> {
        let name = params["name"].as_str().map(str::to_owned);
        if let Some(name) = &name
            && !self.view.lock().expect("view lock").iter().any(|(n, _)| n == name)
        {
            return Err(MethodError::new(400, format!("No updater registered for '{name}'")));
        }
        if self.is_printing() || self.is_busy() {
            return Err(busy_error());
        }
        self.refresh_entries(name.as_deref()).await;
        self.notify_refreshed().await;
        Ok(self.status_json())
    }

    fn status_json(&self) -> Value {
        let version_info: Map<String, Value> = self.view.lock().expect("view lock").iter().cloned().collect();
        json!({
            "version_info": version_info,
            "busy": self.is_busy(),
            "github_rate_limit": Value::Null,
            "github_requests_remaining": Value::Null,
            "github_limit_reset_time": Value::Null,
        })
    }

    async fn notify_refreshed(&self) {
        if let Some(agent) = self.agent() {
            let _ = agent.send_event("update_refreshed", Some(self.status_json())).await;
        }
    }

    /// Checks one entry, or all of them, and stores what it finds.
    async fn refresh_entries(&self, only: Option<&str>) {
        let mut entries = self.entries.lock().await;
        if only.is_none() {
            *self.last_refresh_all.lock().expect("refresh lock") = Some(Instant::now());
        }
        for s in entries.iter_mut().filter(|s| only.is_none_or(|n| n == s.entry.name)) {
            self.refresh_one(s).await;
        }
    }

    async fn refresh_one(&self, s: &mut EntryState) {
        match &s.entry.kind {
            Kind::Git(opts) => {
                let repo = git::refresh(&s.entry, opts, &s.stored.tracking, true).await;
                tracing::info!("Checked {}: {}", s.entry.name, git::summary(&repo));
                s.stored.tracking = git::tracking_of(&repo);
                s.stored.status = repo.status(&s.entry, s.stored.rollback_version.as_deref());
                s.repo = Some(repo);
            }
        }
        s.stored.last_refresh_time = now();
        s.stored.config_hash = s.entry.hash.clone();
        self.set_view(&s.entry.name, s.stored.status.clone());
        if let Some(agent) = self.agent() {
            let item = json!({"namespace": SECTION, "key": s.entry.name, "value": s.stored.to_json()});
            if let Err(e) = agent.call("server.database.post_item", item).await {
                tracing::warn!("Can't save {}'s state: {e}", s.entry.name);
            }
        }
    }

    fn set_view(&self, name: &str, status: Value) {
        let mut view = self.view.lock().expect("view lock");
        match view.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => *v = status,
            None => view.push((name.to_owned(), status)),
        }
    }

    /// Checks the entries when they're due. At startup, that's the ones whose interval passed
    /// or whose config changed. Then, with `enable_auto_refresh`, it sleeps until the next
    /// one is due, and waits for a print to end and for the window to open.
    async fn schedule(self: Arc<Self>) {
        let Some(global) = self.global.lock().expect("global lock").clone() else { return };
        let mut first = true;
        loop {
            if !first && !global.enable_auto_refresh {
                return;
            }
            // Wait for the next entry to be due.
            let due_at = {
                let entries = self.entries.lock().await;
                let pairs: Vec<_> = entries.iter().map(|s| (&s.entry, &s.stored)).collect();
                let changed = entries.iter().any(|s| s.stored.config_hash != s.entry.hash);
                if changed { Some(0.0) } else { next_due(&pairs) }
            };
            let Some(due_at) = due_at else { return };
            let wait = (due_at - now()).max(0.0);
            if wait > 0.0 {
                if first && !global.enable_auto_refresh {
                    return;
                }
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
            first = false;
            // Wait for the print to end, and for the window to open.
            loop {
                if self.is_printing() {
                    self.wake.notified().await;
                    continue;
                }
                if let Some(window) = global.refresh_window {
                    let wait = until_window(window, local_time().await);
                    if wait > 0 {
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }
                }
                if self.is_busy() {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
                break;
            }
            let mut checked = false;
            {
                let mut entries = self.entries.lock().await;
                let t = now();
                for s in entries.iter_mut() {
                    if is_due(&s.entry, &s.stored, t) && !self.is_printing() {
                        self.refresh_one(s).await;
                        checked = true;
                    }
                }
            }
            if checked {
                self.notify_refreshed().await;
            }
        }
    }
}

/// The local time of day, from `date`: reading the time zone without it would take a time zone
/// library, and the window needs it only when a check is due.
async fn local_time() -> (u32, u32, u32) {
    let out = tokio::process::Command::new("date").arg("+%H %M %S").output().await;
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let parts: Vec<u32> = text.split_whitespace().filter_map(|p| p.parse().ok()).collect();
    match parts[..] {
        [h, m, s] => (h, m, s),
        _ => (0, 0, 0),
    }
}

/// An entry's object in `version_info` before its first check since the agent started: the
/// stored one, or what Moonraker shows for a repo it hasn't read yet.
fn view_of(s: &EntryState) -> Value {
    if s.stored.status.is_object() && s.stored.config_hash == s.entry.hash {
        return s.stored.status.clone();
    }
    json!({
        "name": s.entry.name,
        "configured_type": s.entry.kind.configured_type(),
        "channel": s.entry.channel.as_str(),
        "is_valid": false,
        "info_tags": s.entry.info_tags,
        "version": "?",
        "remote_version": "?",
        "warnings": [],
        "anomalies": [],
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config;

    fn entry(hours: &str) -> Entry {
        let g = config::global(&Map::new(), &mut Vec::new());
        let map = json!({"type": "git_repo", "path": "/x", "origin": "o", "refresh_interval": hours});
        config::entry("a", map.as_object().unwrap(), &g).unwrap()
    }

    #[test]
    fn entries_are_due_after_their_interval_or_a_change() {
        let e = entry("24");
        let fresh = Stored { last_refresh_time: 1000.0, config_hash: e.hash.clone(), ..Stored::default() };
        assert!(!is_due(&e, &fresh, 1000.0 + 23.0 * 3600.0));
        assert!(is_due(&e, &fresh, 1000.0 + 24.0 * 3600.0));
        let changed = Stored { config_hash: "old".into(), ..fresh.clone() };
        assert!(is_due(&e, &changed, 1000.0));
        let weekly = entry("168");
        assert_eq!(next_due(&[(&e, &fresh), (&weekly, &fresh)]), Some(1000.0 + 24.0 * 3600.0));
        assert_eq!(next_due(&[]), None);
    }

    #[test]
    fn stale_entries() {
        let stored = json!({"a": {}, "gone": {}});
        assert_eq!(stale_keys(stored.as_object().unwrap(), &[entry("24")]), ["gone"]);
    }

    #[test]
    fn windows() {
        assert_eq!(until_window((0, 5), (3, 0, 0)), 0);
        assert_eq!(until_window((0, 5), (5, 0, 0)), 19 * 3600);
        assert_eq!(until_window((22, 5), (23, 0, 0)), 0);
        assert_eq!(until_window((22, 5), (4, 59, 59)), 0);
        assert_eq!(until_window((22, 5), (21, 30, 0)), 1800);
    }

    #[test]
    fn stored_round_trip() {
        let s = Stored {
            last_refresh_time: 12.5,
            config_hash: "abc".into(),
            status: json!({"version": "v1-0"}),
            tracking: Tracking { branch: Some("main".into()), remote: None },
            rollback_version: Some("v0-9".into()),
        };
        assert_eq!(Stored::from_json(&s.to_json()), s);
    }
}
