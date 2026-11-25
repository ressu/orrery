//! The update manager: its entries, the state it keeps in orrery's database, the checks it
//! runs on a schedule, and the print state that holds them back
//! (docs/design/2026-11-22-update-manager.md).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use orrery_agent::{Agent, MethodError};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

use crate::archive;
use crate::command::{self, Command};
use crate::config::{self, Entry, Global, Kind, Klippy, Project, SECTION};
use crate::deps::{self, Deps};
use crate::executable::{self, Github, Installed, Saved};
use crate::git::{self, RepoState, Rollback, Tracking};
use crate::packagekit::PackageKit;
use crate::plugins;
use crate::release;

/// Moonraker skips a refresh of everything within this long of the last one.
const REFRESH_SPAM: Duration = Duration::from_secs(60);

/// One entry and what's known about it.
struct EntryState {
    entry: Entry,
    stored: Stored,
    /// The last refresh's findings for a git repo, since this agent started.
    repo: Option<RepoState>,
    /// The same for an executable.
    installed: Option<Installed>,
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
    /// Where a rollback goes: the state before the last update, or after the last recover.
    pub rollback: Option<Rollback>,
    /// An executable's release and rollback.
    pub release: Saved,
    /// For the `system` entry: the packages with updates.
    pub packages: Vec<String>,
    /// An `install` or `upgrade` that failed after its update put the files in place.
    pub failed: Option<Failed>,
}

/// The command an update runs after the new version is in place
/// (docs/design/2026-11-24-update-installs.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// After an install from nothing: the entry's `install`.
    Install,
    /// After any other update: the entry's `upgrade`.
    Upgrade,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
        }
    }
}

/// A command that failed. The files stay, and the next update runs it again.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub event: Event,
    /// What was installed when it failed: a release's tag, or a repo's commit. An update that
    /// finds the same runs only the command again.
    pub at: String,
    /// The version, and the one before it, for the command's environment.
    pub version: String,
    pub previous: String,
    pub error: String,
}

impl Failed {
    fn from_json(v: &Value) -> Option<Self> {
        let text = |k: &str| v[k].as_str().map(str::to_owned);
        Some(Self {
            event: match v["event"].as_str()? {
                "install" => Event::Install,
                _ => Event::Upgrade,
            },
            at: text("at")?,
            version: text("version").unwrap_or_default(),
            previous: text("previous").unwrap_or_default(),
            error: text("error").unwrap_or_default(),
        })
    }

    fn to_json(&self) -> Value {
        json!({"event": self.event.as_str(), "at": self.at, "version": self.version, "previous": self.previous, "error": self.error})
    }

    /// The warning the entry shows until the command passes.
    fn warning(&self) -> String {
        let first = self.error.lines().next().unwrap_or_default().trim_end_matches([':', '.']);
        format!("{first}. Update again to run it again.")
    }
}

/// Shows a failed command in the entry's status: still valid, so clients keep the update
/// button, with a version older than any, and a warning that says what failed.
fn mark_failed(status: &mut Value, failed: &Failed) {
    status["version"] = json!(executable::UNKNOWN_VERSION);
    status["is_valid"] = json!(true);
    let warning = failed.warning();
    match status["warnings"].as_array_mut() {
        Some(w) => w.push(json!(warning)),
        None => status["warnings"] = json!([warning]),
    }
}

impl Stored {
    pub fn from_json(v: &Value) -> Self {
        let text = |k: &str| v[k].as_str().map(str::to_owned);
        Self {
            last_refresh_time: v["last_refresh_time"].as_f64().unwrap_or(0.0),
            config_hash: text("config_hash").unwrap_or_default(),
            status: v.get("status").cloned().unwrap_or(Value::Null),
            tracking: Tracking { branch: text("branch"), remote: text("remote") },
            rollback: text("rollback_commit").map(|commit| Rollback {
                commit,
                branch: text("rollback_branch").unwrap_or_else(|| "?".into()),
                version: text("rollback_version").unwrap_or_else(|| "?".into()),
            }),
            release: Saved::from_json(&v["release"]),
            packages: v["packages"]
                .as_array()
                .map(|l| l.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            failed: Failed::from_json(&v["failed"]),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "last_refresh_time": self.last_refresh_time,
            "config_hash": self.config_hash,
            "status": self.status,
            "branch": self.tracking.branch,
            "remote": self.tracking.remote,
            "rollback_commit": self.rollback.as_ref().map(|r| &r.commit),
            "rollback_branch": self.rollback.as_ref().map(|r| &r.branch),
            "rollback_version": self.rollback.as_ref().map(|r| &r.version),
            "release": self.release.to_json(),
            "packages": self.packages,
            "failed": self.failed.as_ref().map(Failed::to_json),
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

/// The anomalies a refresh found that the last one didn't, to log each one once. They're
/// logged even when `report_anomalies` hides them from frontends.
fn new_anomalies<'a>(before: &[String], now: &'a [String]) -> Vec<&'a String> {
    now.iter().filter(|a| !before.contains(a)).collect()
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
    /// Config problems, for `server.info`: the config's, then the Klipper entry's.
    warnings: Mutex<Vec<String>>,
    config_warnings: Mutex<Vec<String>>,
    /// The `klipper` and `kalico` sections, for Klipper's entry once Klippy says where it runs.
    klipper_sections: Mutex<Vec<config::KlipperSection>>,
    /// Every `git_repo` entry's name and path, to find the plugins linked into Klipper.
    git_paths: Mutex<Vec<(String, std::path::PathBuf)>>,
    /// The plugin links in Klipper's folder, from its entry's last refresh.
    plugin_links: Mutex<plugins::Links>,
    /// Commands waiting for the one that runs. While one waits, restarts wait for it too.
    waiting: AtomicUsize,
    /// Services to restart once the updates are done, in the order they were added.
    pending: Mutex<Vec<String>>,
    /// Whether a task waits for a print to end, to restart the pending services.
    restart_after_print: AtomicBool,
    /// Whether orrery restarts once the command's last message is sent, which its restart
    /// cuts off, since it stops this agent.
    restart_orrery: AtomicBool,
    next_proc_id: AtomicU64,
    /// GitHub, for executables; the error when HTTPS can't be set up.
    github: Result<Github, String>,
}

/// One command's progress messages, sent as `notify_update_response` and logged.
struct Progress {
    agent: Option<Agent>,
    application: String,
    proc_id: u64,
}

impl Progress {
    async fn send(&self, message: &str, complete: bool) {
        tracing::info!("{message}");
        self.notify(message, complete).await;
    }

    /// A line of a program's output: in the update dialog, and only in the debug log.
    async fn output(&self, line: &str) {
        tracing::debug!("{line}");
        self.notify(line, false).await;
    }

    async fn notify(&self, message: &str, complete: bool) {
        if let Some(agent) = &self.agent {
            let data = json!({"message": message, "application": self.application, "proc_id": self.proc_id, "complete": complete});
            let _ = agent.send_event("update_response", Some(data)).await;
        }
    }
}

/// Why a command stopped: the messages for the update dialog, the last one completing it, and
/// the error for the request.
#[derive(Debug)]
struct Failure {
    messages: Vec<String>,
    code: i64,
    error: String,
}

impl Failure {
    fn new(code: i64, error: impl Into<String>) -> Self {
        Self { messages: Vec::new(), code, error: error.into() }
    }
}

/// A boolean parameter, as text or JSON.
fn flag(params: &Value, key: &str) -> bool {
    match &params[key] {
        Value::Bool(b) => *b,
        Value::String(s) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Why a repo isn't valid, for the message that refuses its update.
fn invalid_reason(repo: &RepoState) -> String {
    if repo.diverged {
        format!(": it has diverged from {}/{}, and a recover resets it", repo.remote, repo.branch)
    } else if repo.warnings.is_empty() {
        String::new()
    } else {
        format!(": {}", repo.warnings.join("; "))
    }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn busy_error() -> MethodError {
    MethodError::new(503, "Server is busy, cannot perform refresh")
}

impl Manager {
    pub fn new() -> Arc<Self> {
        Self::with_github(Github::new())
    }

    fn with_github(github: Result<Github, String>) -> Arc<Self> {
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
            config_warnings: Mutex::new(Vec::new()),
            klipper_sections: Mutex::new(Vec::new()),
            git_paths: Mutex::new(Vec::new()),
            plugin_links: Mutex::new(plugins::Links::default()),
            waiting: AtomicUsize::new(0),
            pending: Mutex::new(Vec::new()),
            restart_after_print: AtomicBool::new(false),
            restart_orrery: AtomicBool::new(false),
            next_proc_id: AtomicU64::new(now() as u64),
            github,
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
        for name in ["upgrade", "client", "full", "system", "recover", "rollback", "klipper", "moonraker"] {
            agent.register_endpoint(name, &["POST"]).await?;
        }

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
                        tokio::spawn(async move {
                            m.subscribe().await;
                            m.set_klipper_entry().await;
                        });
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
            let m = self.clone();
            tokio::spawn(async move { m.set_klipper_entry().await });
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
        // Moonraker lists `system` first, and `full` updates it first.
        let mut entries = Vec::new();
        if global.enable_system_updates {
            entries.push(config::system_entry(&global));
        }
        let mut klipper_sections = Vec::new();
        let names = agent.config_names(None).await?;
        // orrery's own entry comes next, where Moonraker lists its own.
        let section = format!("{SECTION} orrery");
        let (map, from_orrery_toml) = if names.iter().any(|n| n == "orrery") {
            (
                agent.config(Some(&section)).await?.unwrap_or_default(),
                agent.config_from_orrery_toml(Some(&section)).await?,
            )
        } else {
            (Map::new(), Vec::new())
        };
        let args = release::orrery_args();
        match config::orrery_entry(release::own_dir(), &map, &from_orrery_toml, &args, &global, &mut warnings) {
            Ok(Some(e)) => {
                if let Some(problem) = release::problem() {
                    tracing::info!("orrery {}: {problem}", orrery_build::VERSION);
                }
                entries.push(e);
            }
            Ok(None) => {}
            Err(w) => warnings.push(w),
        }
        for name in names {
            if name == "orrery" {
                continue;
            }
            let map = agent.config(Some(&format!("{SECTION} {name}"))).await?.unwrap_or_default();
            if config::is_klipper_section(&name) {
                let from_orrery_toml = agent.config_from_orrery_toml(Some(&format!("{SECTION} {name}"))).await?;
                klipper_sections.push(config::KlipperSection { name, options: map, from_orrery_toml });
                continue;
            }
            let section = format!("{SECTION} {name}");
            let from_orrery_toml = agent.config_from_orrery_toml(Some(&section)).await?;
            let entry = config::entry(&name, &map, &global).and_then(|mut e| {
                config::own_options(&section, &map, &from_orrery_toml, &mut e, &mut warnings)?;
                Ok(e)
            });
            match entry {
                Ok(e) => {
                    warnings.extend(e.services_warning());
                    warnings.extend(e.channel_warning.clone());
                    entries.push(e);
                }
                Err(w) => warnings.push(w),
            }
        }
        // The reply is `{namespace, key, value}`, with the whole namespace as the value. A
        // namespace that isn't there yet is an error.
        let stored = match agent.call("server.database.get_item", json!({"namespace": SECTION})).await {
            Ok(reply) => reply["value"].as_object().cloned().unwrap_or_default(),
            Err(_) => Map::new(),
        };
        // Entries that are gone from the config leave no state behind. Klipper's entry comes
        // once Klippy connects, and cleans up after itself.
        for key in stale_keys(&stored, &entries).into_iter().filter(|k| !config::is_klipper_section(k)) {
            let _ = agent.call("server.database.delete_item", json!({"namespace": SECTION, "key": key})).await;
        }
        let mut states: Vec<EntryState> = entries
            .into_iter()
            .map(|entry| {
                let stored = stored.get(&entry.name).map(Stored::from_json).unwrap_or_default();
                EntryState { entry, stored, repo: None, installed: None }
            })
            .collect();
        // orrery's programs may have changed since the stored status, by an update and the
        // restart after it, or from a shell.
        for s in states.iter_mut().filter(|s| release::is_own(&s.entry)) {
            s.installed = Some(executable::installed_release(&s.entry).await);
            release::restarted(&mut s.stored.release, valid_version(s.installed.as_ref()));
            s.stored.status = own_status(s);
        }
        tracing::info!(
            "{} update manager entries, checked every {} hours{}",
            states.len(),
            global.refresh_interval.as_secs() / 3600,
            if global.enable_auto_refresh { "" } else { " only at startup" }
        );
        *self.view.lock().expect("view lock") = states.iter().map(|s| (s.entry.name.clone(), view_of(s))).collect();
        *self.git_paths.lock().expect("paths lock") = states
            .iter()
            .filter_map(|s| match &s.entry.kind {
                Kind::Git(o) => Some((s.entry.name.clone(), o.path.clone())),
                _ => None,
            })
            .collect();
        *self.entries.lock().await = states;
        *self.global.lock().expect("global lock") = Some(global);
        *self.klipper_sections.lock().expect("sections lock") = klipper_sections;
        agent.set_warnings(&warnings).await?;
        *self.config_warnings.lock().expect("warnings lock") = warnings.clone();
        *self.warnings.lock().expect("warnings lock") = warnings;
        Ok(())
    }

    /// Adds, changes, or removes Klipper's entry from what Klippy reports, as Moonraker's
    /// `_set_klipper_repo` does each time Klippy connects. A new entry is checked at once.
    async fn set_klipper_entry(self: &Arc<Self>) {
        let Some(agent) = self.agent() else { return };
        let Some(global) = self.global.lock().expect("global lock").clone() else { return };
        let info = match agent.call("printer.info", Value::Null).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("Can't ask Klippy where it runs, so there's no entry for it: {e}");
                return;
            }
        };
        let (Some(path), Some(python)) = (info["klipper_path"].as_str(), info["python_path"].as_str()) else {
            tracing::warn!("Klippy doesn't say where it runs, so there's no entry for it");
            return;
        };
        let path = std::path::PathBuf::from(path);
        let remote = git::remote_url(&path).await;
        let k =
            Klippy { project: Project::detect(info["app"].as_str(), remote.as_deref()), path, python: python.into() };
        let sections = self.klipper_sections.lock().expect("sections lock").clone();
        let mut warnings = Vec::new();
        let entry = match config::klipper_entry(&k, &sections, &global, &mut warnings) {
            Ok(e) => e,
            Err(w) => {
                warnings.push(w);
                None
            }
        };
        let mut all = self.config_warnings.lock().expect("warnings lock").clone();
        all.extend(warnings);
        if *self.warnings.lock().expect("warnings lock") != all {
            let _ = agent.set_warnings(&all).await;
            *self.warnings.lock().expect("warnings lock") = all;
        }

        let mut entries = self.entries.lock().await;
        let old = entries.iter().position(|s| s.entry.klippy.is_some());
        if let (Some(i), Some(e)) = (old, &entry)
            && entries[i].entry == *e
        {
            return;
        }
        if let Some(i) = old {
            let gone = entries.remove(i);
            self.view.lock().expect("view lock").retain(|(n, _)| *n != gone.entry.name);
            if entry.as_ref().is_none_or(|e| e.name != gone.entry.name) {
                let _ = agent
                    .call("server.database.delete_item", json!({"namespace": SECTION, "key": gone.entry.name}))
                    .await;
            }
        }
        let Some(entry) = entry else { return };
        tracing::info!("Klippy runs {} from {}", entry.name, k.path.display());
        let stored =
            match agent.call("server.database.get_item", json!({"namespace": SECTION, "key": entry.name})).await {
                Ok(reply) => Stored::from_json(&reply["value"]),
                Err(_) => Stored::default(),
            };
        // After `system` and orrery's own entry, where Moonraker lists it.
        let at =
            entries.iter().take_while(|s| matches!(s.entry.kind, Kind::System) || release::is_own(&s.entry)).count();
        let mut s = EntryState { entry, stored, repo: None, installed: None };
        {
            let mut view = self.view.lock().expect("view lock");
            let at_view = at.min(view.len());
            view.insert(at_view, (s.entry.name.clone(), view_of(&s)));
        }
        // The links decide restarts, so they're found now even when no refresh is due.
        if let Kind::Git(o) = &s.entry.kind {
            let others = self.git_paths.lock().expect("paths lock").clone();
            *self.plugin_links.lock().expect("links lock") = plugins::find(&o.path, &others).await;
        }
        let due = is_due(&s.entry, &s.stored, now()) || s.stored.config_hash != s.entry.hash;
        if due && !self.is_printing() {
            self.refresh_one(&mut s).await;
        }
        entries.insert(at, s);
        drop(entries);
        if due {
            self.notify_refreshed().await;
        }
        self.wake.notify_one();
    }

    /// The name of Klipper's entry, `klipper` or `kalico`, once Klippy has said where it runs.
    pub fn klipper_entry_name(&self) -> Option<String> {
        self.view.lock().expect("view lock").iter().map(|(n, _)| n).find(|n| config::is_klipper_section(n)).cloned()
    }

    /// `machine.update.status`: every entry's state. With `refresh`, everything is checked
    /// first, unless a print or a command is running, or the last check was a minute ago;
    /// then the request is answered without a check, as Moonraker does.
    pub async fn status(self: &Arc<Self>, params: &Value) -> Result<Value, MethodError> {
        let refresh = flag(params, "refresh");
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
        let rate = self.github.as_ref().map(Github::rate).unwrap_or_default();
        json!({
            "version_info": version_info,
            "busy": self.is_busy(),
            "github_rate_limit": rate.limit,
            "github_requests_remaining": rate.remaining,
            "github_limit_reset_time": rate.reset,
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
        self.read_repo(s, true).await;
        let summary = match (&s.repo, &s.installed) {
            (Some(repo), _) => git::summary(repo),
            (_, Some(i)) if s.stored.release.remote_version == i.version => format!("up to date at {}", i.version),
            (_, Some(i)) => format!("at {}, {} available", i.version, s.stored.release.remote_version),
            _ => match s.stored.packages.len() {
                1 => "1 package update".into(),
                n => format!("{n} package updates"),
            },
        };
        tracing::info!("Checked {}: {summary}", s.entry.name);
    }

    /// Reads the entry's state, checking for updates first when `fetch` is set, and stores
    /// what it finds: a git repo, or an executable and its newest release.
    async fn read_repo(&self, s: &mut EntryState, fetch: bool) {
        let opts = match &s.entry.kind {
            Kind::Git(opts) => opts,
            Kind::System => {
                if fetch {
                    match PackageKit::connect().await {
                        Ok(pk) => match pk.updates(None).await {
                            Ok((packages, refresh_error)) => {
                                if let Some(e) = refresh_error {
                                    crate::warn_first_line(
                                        "Can't refresh the package lists, so the updates come from the old lists: ",
                                        &e,
                                    );
                                }
                                s.stored.packages = packages.iter().map(|p| p.name().to_owned()).collect();
                            }
                            Err(e) => crate::warn_first_line("Can't check the system packages: ", &e),
                        },
                        Err(e) => crate::warn_first_line("Can't check the system packages: ", &e),
                    }
                }
                self.save(s).await;
                return;
            }
            Kind::Executable(_) | Kind::Archive(_) => {
                let installed = executable::installed_release(&s.entry).await;
                if release::is_own(&s.entry) {
                    release::restarted(&mut s.stored.release, valid_version(Some(&installed)));
                }
                if fetch {
                    match &self.github {
                        Ok(gh) => executable::refresh(gh, &s.entry, &installed, &mut s.stored.release).await,
                        Err(e) => s.stored.release.last_error.clone_from(e),
                    }
                }
                if s.stored.release.rollback_version == "?" && installed.valid {
                    s.stored.release.rollback_version.clone_from(&installed.version);
                    s.stored.release.rollback_repo.clone_from(&installed.repo);
                }
                s.installed = Some(installed);
                self.save(s).await;
                return;
            }
        };
        let mut repo = git::refresh(&s.entry, opts, &s.stored.tracking, fetch).await;
        if let (Some(project), true) = (s.entry.klippy, repo.repo_detected) {
            let others = self.git_paths.lock().expect("paths lock").clone();
            let links = plugins::find(&opts.path, &others).await;
            repo.set_plugins(project.title(), &links);
            *self.plugin_links.lock().expect("links lock") = links;
        }
        s.stored.tracking = git::tracking_of(&repo);
        let before = s.repo.as_ref().map(|r| r.anomalies.as_slice()).unwrap_or_default();
        for anomaly in new_anomalies(before, &repo.anomalies) {
            tracing::info!("{}: {anomaly}", s.entry.name);
        }
        s.repo = Some(repo);
        self.save(s).await;
    }

    /// Stores the entry's state, with its object in `version_info`.
    async fn save(&self, s: &mut EntryState) {
        if let Some(repo) = &s.repo {
            s.stored.status = repo.status(&s.entry, s.stored.rollback.as_ref().map(|r| r.version.as_str()));
        }
        if s.installed.is_some() {
            s.stored.status = own_status(s);
        }
        if let Kind::System = s.entry.kind {
            s.stored.status = system_status(&s.stored.packages);
        }
        if let Some(failed) = &s.stored.failed {
            mark_failed(&mut s.stored.status, failed);
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

    fn has_entry(&self, name: &str) -> bool {
        self.view.lock().expect("view lock").iter().any(|(n, _)| n == name)
    }

    fn not_available(name: &str) -> MethodError {
        MethodError::new(404, format!("Updater {name} not available"))
    }

    /// `machine.update.upgrade` and `client`: updates the entry `name`, or everything
    /// without it. `full` is the same without a name.
    pub async fn update(self: &Arc<Self>, params: &Value) -> Result<Value, MethodError> {
        if self.is_printing() {
            return Err(MethodError::new(503, "Update Refused: Klippy is printing"));
        }
        let name = params["name"].as_str().map(str::to_owned);
        if let Some(name) = &name
            && !self.has_entry(name)
        {
            return Err(Self::not_available(name));
        }
        self.run(name.as_deref().unwrap_or("full"), async |m, entries, p| match &name {
            Some(name) => {
                let s = entries.iter_mut().find(|s| &s.entry.name == name).expect("checked above");
                match m.update_entry(s, p).await {
                    Ok(true) => Ok(Some(s.entry.finished())),
                    // Moonraker says nothing for an entry that's up to date.
                    Ok(false) => Ok(None),
                    Err(e) => Err(Failure { messages: vec![format!("Error updating {name}: {}", e.error)], ..e }),
                }
            }
            None => {
                p.send("Preparing full software update...", false).await;
                for s in entries.iter_mut() {
                    // A development build, or one without a repository, isn't updated by
                    // name either, but it's no reason to stop the others.
                    if release::is_own(&s.entry) && release::problem().is_some() {
                        continue;
                    }
                    if let Err(e) = m.update_entry(s, p).await {
                        let messages = vec![format!("Error updating {}: {}", s.entry.name, e.error)];
                        return Err(Failure { messages, ..e });
                    }
                }
                Ok(Some("Full Update Complete".into()))
            }
        })
        .await
    }

    /// `machine.update.recover`: a soft recover resets the repo; a hard one clones it again.
    pub async fn recover(self: &Arc<Self>, params: &Value) -> Result<Value, MethodError> {
        if self.is_printing() {
            return Err(MethodError::new(400, "Recovery Attempt Refused: Klippy is printing"));
        }
        let name = params["name"].as_str().ok_or_else(|| MethodError::new(400, "No data for argument: name"))?;
        if !self.has_entry(name) {
            return Err(Self::not_available(name));
        }
        if self.view.lock().expect("view lock").iter().any(|(n, v)| n == name && v["configured_type"] != "git_repo") {
            return Err(MethodError::new(400, format!("Updater {name} is not a Git Repo Type")));
        }
        let hard = flag(params, "hard");
        let all_deps = flag(params, "update_deps");
        let name = name.to_owned();
        self.run(&format!("recover_{name}"), async |m, entries, p| {
            let s = entries.iter_mut().find(|s| s.entry.name == name).expect("checked above");
            m.recover_entry(s, hard, all_deps, p)
                .await
                .map(|()| Some(format!("Git Repo {name}: Reinstall Complete")))
                .map_err(|e| Failure { messages: vec![format!("Error Recovering {name}"), e.error.clone()], ..e })
        })
        .await
    }

    /// `machine.update.rollback`: back to the commit before the last update.
    pub async fn rollback(self: &Arc<Self>, params: &Value) -> Result<Value, MethodError> {
        if self.is_printing() {
            return Err(MethodError::new(400, "Rollback Attempt Refused: Klippy is printing"));
        }
        let name = params["name"].as_str().ok_or_else(|| MethodError::new(400, "No data for argument: name"))?;
        if !self.has_entry(name) {
            return Err(Self::not_available(name));
        }
        let name = name.to_owned();
        self.run(&format!("rollback_{name}"), async |m, entries, p| {
            let s = entries.iter_mut().find(|s| s.entry.name == name).expect("checked above");
            let done = m
                .rollback_entry(s, p)
                .await
                .map_err(|e| Failure { messages: vec![format!("Error Rolling Back {name}"), e.error.clone()], ..e })?;
            Ok(Some(format!("{}Rollback {}", s.entry.prefix(), if done { "Complete" } else { "not performed" })))
        })
        .await
    }

    /// Runs one command: after the one before it, with `busy` set, and with the restarts it
    /// leaves pending at the end. The command returns the message that completes it.
    async fn run<F>(self: &Arc<Self>, application: &str, command: F) -> Result<Value, MethodError>
    where
        F: AsyncFnOnce(&Arc<Self>, &mut Vec<EntryState>, &Progress) -> Result<Option<String>, Failure>,
    {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let mut entries = self.entries.lock().await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        *self.busy.lock().expect("busy lock") = true;
        let p = Progress {
            agent: self.agent(),
            application: application.to_owned(),
            proc_id: self.next_proc_id.fetch_add(1, Ordering::Relaxed),
        };
        let result = command(self, &mut entries, &p).await;
        self.restart_pending(&p).await;
        drop(entries);
        *self.busy.lock().expect("busy lock") = false;
        self.notify_refreshed().await;
        let answer = match result {
            Ok(done) => {
                if let Some(message) = done {
                    p.send(&message, true).await;
                }
                Ok(json!("ok"))
            }
            Err(f) => {
                for (i, message) in f.messages.iter().enumerate() {
                    p.send(message, i + 1 == f.messages.len()).await;
                }
                Err(MethodError::new(f.code, f.error))
            }
        };
        // Last, since it stops this agent. The answer to the request may not arrive; the
        // update dialog is complete by now.
        if self.restart_orrery.swap(false, Ordering::SeqCst) {
            self.restart_self(&p).await;
        }
        answer
    }

    /// Updates one entry to the version a fresh check finds. Returns false when it's up to
    /// date.
    async fn update_entry(&self, s: &mut EntryState, p: &Progress) -> Result<bool, Failure> {
        match s.entry.kind {
            Kind::Archive(_) if release::is_own(&s.entry) => return self.update_own(s, p, false).await,
            Kind::Executable(_) | Kind::Archive(_) => return self.update_executable(s, p, None).await,
            Kind::System => return self.update_system(s, p).await,
            Kind::Git(_) => {}
        }
        let name = s.entry.name.clone();
        self.read_repo(s, true).await;
        let repo = s.repo.as_ref().expect("just read");
        if !repo.is_valid() {
            return Err(Failure::new(400, format!("Update aborted, repo not valid{}", invalid_reason(repo))));
        }
        // A command that failed on this commit runs again, without a pull.
        let failed = s.stored.failed.clone();
        if let Some(f) = failed.as_ref().filter(|f| f.at == repo.current_commit) {
            return self.retry(s, f.clone(), p).await;
        }
        if repo.missing {
            return self.install_repo(s, p).await;
        }
        if repo.current_commit == repo.upstream_commit {
            return Ok(false);
        }
        p.send(&format!("Updating Application {name}..."), false).await;
        p.send(&format!("Git Repo {name}: Updating Repo..."), false).await;
        let Kind::Git(opts) = &s.entry.kind else { unreachable!("an executable updates above") };
        if let Some(project) = s.entry.klippy {
            // Git would stop halfway on a file the update adds that's already there.
            let added = plugins::added(&opts.path, &repo.current_commit, &repo.upstream_commit)
                .await
                .map_err(|e| Failure::new(500, e))?;
            let links = self.plugin_links.lock().expect("links lock").clone();
            let collisions = plugins::collisions(project.title(), &added, &links);
            if !collisions.is_empty() {
                return Err(Failure::new(400, collisions.join(" ")));
            }
        }
        let rollback = Rollback::of(repo);
        let before = Deps::read(&s.entry.kind.deps()).await;
        git::fast_forward(opts, repo).await.map_err(|e| Failure::new(500, e))?;
        self.update_deps(s, &before, false, p).await;
        if let Some(preflight) = &s.entry.preflight {
            p.send(&format!("Git Repo {name}: Running the preflight check..."), false).await;
            let checked = match preflight {
                Command::Shell(line) => command::preflight_shell(line, &opts.path, None).await,
                Command::Words(words) => {
                    command::preflight(&words[0], &opts.path.join(&words[0]), &words[1..], &opts.path).await
                }
            };
            if let Err(e) = checked {
                // The update doesn't take effect: the repo goes back, and nothing restarts.
                let reset = git::reset_to(opts, &rollback.commit).await;
                self.read_repo(s, false).await;
                let back = match reset {
                    Ok(()) => format!("{name} is back at {}.", rollback.version),
                    Err(r) => format!("{name} couldn't go back to {}: {r}", rollback.version),
                };
                return Err(Failure::new(
                    500,
                    format!("{e}\n{back} Python packages that the update installed stay installed."),
                ));
            }
        }
        s.stored.rollback = Some(rollback.clone());
        self.read_repo(s, false).await;
        let repo = s.repo.as_ref().expect("just read");
        let (at, version) = (repo.current_commit.clone(), repo.current.short());
        // An install that failed stays an install.
        let event = if failed.is_some_and(|f| f.event == Event::Install) { Event::Install } else { Event::Upgrade };
        self.run_command(s, event, (&at, &version, &rollback.version), p).await?;
        self.save(s).await;
        self.add_pending(&s.entry);
        Ok(true)
    }

    async fn recover_entry(&self, s: &mut EntryState, hard: bool, all_deps: bool, p: &Progress) -> Result<(), Failure> {
        let name = s.entry.name.clone();
        p.send(&format!("Git Repo {name}: Attempting Repo Recovery..."), false).await;
        self.read_repo(s, true).await;
        let Kind::Git(opts) = &s.entry.kind else { unreachable!("recover refuses other types") };
        let before = Deps::read(&s.entry.kind.deps()).await;
        let repo = s.repo.as_ref().expect("just read");
        if hard {
            p.send(&format!("Git Repo {name}: Starting Clone Recovery..."), false).await;
            if let Err(e) = git::hard_recover(opts, repo).await {
                p.send(&format!("Git Repo {name}: Git Clone Failed"), false).await;
                return Err(Failure::new(500, e));
            }
            p.send(&format!("Git Repo {name}: Git Clone Complete"), false).await;
        } else {
            p.send(&format!("Git Repo {name}: Resetting Git Repo..."), false).await;
            git::soft_recover(opts, repo).await.map_err(|e| Failure::new(500, e))?;
        }
        // The tracking that a detached repo had is gone with the reset.
        s.stored.tracking = Tracking::default();
        self.read_repo(s, false).await;
        let repo = s.repo.as_ref().expect("just read");
        s.stored.rollback = Some(Rollback::of(repo));
        self.save(s).await;
        let repo = s.repo.as_ref().expect("just read");
        if repo.current.dirty || !repo.is_valid() {
            return Err(Failure::new(500, "Recovery attempt failed, repo state not pristine"));
        }
        // A new clone is a fresh install of the repo, so it runs the entry's install.
        if hard && s.entry.install.is_some() {
            return self.after_clone(s, p).await;
        }
        self.update_deps(s, &before, all_deps, p).await;
        self.add_pending(&s.entry);
        Ok(())
    }

    async fn rollback_entry(&self, s: &mut EntryState, p: &Progress) -> Result<bool, Failure> {
        match s.entry.kind {
            Kind::Archive(_) if release::is_own(&s.entry) => return self.update_own(s, p, true).await,
            Kind::Executable(_) | Kind::Archive(_) => return self.rollback_executable(s, p).await,
            Kind::System => return Err(Failure::new(400, "Rollback not available for system")),
            Kind::Git(_) => {}
        }
        self.read_repo(s, false).await;
        let repo = s.repo.as_ref().expect("just read");
        // Moonraker resets a changed repo here, and the changes are lost.
        if repo.current.dirty {
            return Err(Failure::new(400, "Rollback aborted, repo has been modified. A recover resets it."));
        }
        let Some(rollback) = s.stored.rollback.clone() else {
            return Err(Failure::new(400, "Incomplete rollback data stored, cannot rollback"));
        };
        let Kind::Git(opts) = &s.entry.kind else { unreachable!("an executable rolls back above") };
        let before = Deps::read(&s.entry.kind.deps()).await;
        let done = git::rollback(opts, repo, &rollback).await.map_err(|e| Failure::new(500, e))?;
        if done {
            self.update_deps(s, &before, false, p).await;
            self.read_repo(s, false).await;
            self.add_pending(&s.entry);
        }
        Ok(done)
    }

    /// Installs what the entry's dependency files list now and didn't list `before`, or all
    /// of it with `all`. A failure is reported, and the update goes on, as in Moonraker.
    async fn update_deps(&self, s: &EntryState, before: &Deps, all: bool, p: &Progress) {
        self.update_some_deps(s, before, all, (true, true), p).await;
    }

    /// Runs the entry's `install` or `upgrade`, in its folder, as orrery's user, with its output
    /// in the update dialog. `at` is what's installed now, `version` and `previous` the versions
    /// for its environment. A failure is kept, and shown, until a later update runs it again.
    async fn run_command(
        &self,
        s: &mut EntryState,
        event: Event,
        (at, version, previous): (&str, &str, &str),
        p: &Progress,
    ) -> Result<(), Failure> {
        let cmd = match event {
            Event::Install => s.entry.install.as_ref().and_then(command::Install::command),
            Event::Upgrade => s.entry.upgrade.as_ref(),
        };
        let Some(cmd) = cmd.cloned() else {
            s.stored.failed = None;
            return Ok(());
        };
        let path = s.entry.kind.path().expect("an entry with a folder").to_path_buf();
        let what = format!("{} command", event.as_str());
        p.send(&format!("{}Running the {what}...", s.entry.prefix()), false).await;
        let env = [
            ("ORRERY_ENTRY".to_owned(), s.entry.name.clone()),
            ("ORRERY_PATH".to_owned(), path.display().to_string()),
            ("ORRERY_VERSION".to_owned(), version.to_owned()),
            ("ORRERY_PREVIOUS_VERSION".to_owned(), previous.to_owned()),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let spec = cmd.spec(&path);
        let work = async move {
            let run = command::Run {
                what: &what,
                dir: &path,
                bin: None,
                env: &env,
                limit: command::COMMAND_LIMIT,
                out: Some(&tx),
            };
            command::run(&spec, &run).await
        };
        let forward = async {
            while let Some(line) = rx.recv().await {
                p.output(&line).await;
            }
        };
        let (ran, ()) = tokio::join!(work, forward);
        match ran {
            Ok(()) => {
                s.stored.failed = None;
                Ok(())
            }
            Err(e) => {
                let failed = Failed {
                    event,
                    at: at.to_owned(),
                    version: version.to_owned(),
                    previous: previous.to_owned(),
                    error: e.clone(),
                };
                s.stored.failed = Some(failed);
                self.save(s).await;
                Err(Failure::new(
                    500,
                    format!(
                        "{e}
The new files stay in place. Update again to run it again."
                    ),
                ))
            }
        }
    }

    /// Clones a git entry's repository into its folder, which isn't there, for an entry whose
    /// `install` allows it: the clone, its system packages, `install`, which may make the
    /// virtualenv, and its Python packages.
    async fn install_repo(&self, s: &mut EntryState, p: &Progress) -> Result<bool, Failure> {
        let name = s.entry.name.clone();
        let Kind::Git(opts) = &s.entry.kind else { unreachable!("a git entry") };
        p.send(&format!("Installing Application {name}..."), false).await;
        p.send(&format!("Git Repo {name}: Cloning {}...", opts.origin), false).await;
        let repo = s.repo.as_ref().expect("just read");
        git::clone_fresh(opts, repo).await.map_err(|e| Failure::new(500, e))?;
        p.send(&format!("Git Repo {name}: Git Clone Complete"), false).await;
        self.after_clone(s, p).await?;
        Ok(true)
    }

    /// What follows a new clone: the system packages, `install`, the Python packages, and the
    /// restart.
    async fn after_clone(&self, s: &mut EntryState, p: &Progress) -> Result<(), Failure> {
        s.stored.tracking = Tracking::default();
        self.read_repo(s, false).await;
        let repo = s.repo.as_ref().expect("just read");
        s.stored.rollback = Some(Rollback::of(repo));
        let (at, version) = (repo.current_commit.clone(), repo.current.short());
        let none = Deps::default();
        self.update_some_deps(s, &none, true, (true, false), p).await;
        self.run_command(s, Event::Install, (&at, &version, ""), p).await?;
        self.update_some_deps(s, &none, true, (false, true), p).await;
        self.save(s).await;
        self.add_pending(&s.entry);
        Ok(())
    }

    /// Runs a failed command again, when the update finds what it failed on.
    async fn retry(&self, s: &mut EntryState, failed: Failed, p: &Progress) -> Result<bool, Failure> {
        p.send(&format!("{}Updating...", s.entry.prefix()), false).await;
        self.run_command(s, failed.event, (&failed.at, &failed.version, &failed.previous), p).await?;
        // A git install that failed stopped before its Python packages.
        if failed.event == Event::Install && matches!(s.entry.kind, Kind::Git(_)) {
            self.update_some_deps(s, &Deps::default(), true, (false, true), p).await;
        }
        self.save(s).await;
        self.add_pending(&s.entry);
        Ok(true)
    }

    /// [`Self::update_deps`], for the system packages, the Python packages, or both.
    async fn update_some_deps(
        &self,
        s: &EntryState,
        before: &Deps,
        all: bool,
        (system, python): (bool, bool),
        p: &Progress,
    ) {
        let files = s.entry.kind.deps();
        let opts = &files;
        let prefix = s.entry.prefix();
        let mut added = before.added(&Deps::read(opts).await, all);
        if !system {
            added.packages.clear();
        }
        if !python {
            added.modules.clear();
        }
        if added.packages.is_empty() && added.modules.is_empty() {
            return;
        }
        // The installs report their output through a channel, which this passes on as they
        // run.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let work = async move {
            if !added.packages.is_empty() {
                p.send(&format!("{prefix}Installing system dependencies..."), false).await;
                let installed = match PackageKit::connect().await {
                    Ok(pk) => pk.install(&added.packages, &tx).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = installed {
                    p.send(&format!("{prefix}Error updating packages: {e}"), false).await;
                }
            }
            if !added.modules.is_empty()
                && let Some(requirements) = deps::requirements_file(opts)
            {
                p.send(&format!("{prefix}Updating python packages..."), false).await;
                if let Err(e) = deps::pip_install(opts, &requirements, &tx).await {
                    p.send(&format!("{prefix}Error updating python requirements: {e}"), false).await;
                }
            }
        };
        let forward = async {
            while let Some(line) = rx.recv().await {
                p.output(&line).await;
            }
        };
        tokio::join!(work, forward);
    }

    /// Updates an executable to the newest release, or, with `to`, to the version and download
    /// a rollback found. Returns false when it's there already.
    async fn update_executable(
        &self,
        s: &mut EntryState,
        p: &Progress,
        to: Option<(String, executable::Asset, Option<executable::Asset>)>,
    ) -> Result<bool, Failure> {
        let prefix = s.entry.prefix();
        let rolling_back = to.is_some();
        if !rolling_back {
            self.read_repo(s, true).await;
        }
        let installed = s.installed.clone().expect("just read");
        if !installed.valid {
            return Err(Failure::new(400, format!("{prefix}Invalid install detected, aborting update")));
        }
        // A command that failed on this version runs again, without a new download.
        let failed = s.stored.failed.clone();
        if !rolling_back && let Some(f) = failed.as_ref().filter(|f| f.at == installed.version) {
            return self.retry(s, f.clone(), p).await;
        }
        // An install that failed stays an install.
        let event = if installed.fresh || failed.as_ref().is_some_and(|f| f.event == Event::Install) {
            Event::Install
        } else {
            Event::Upgrade
        };
        let (version, asset, checksums) = match to {
            Some(target) => target,
            None => {
                let saved = &s.stored.release;
                if saved.remote_version == "?" {
                    return Err(Failure::new(400, format!("{prefix}Unable to locate update")));
                }
                if saved.remote_version == installed.version {
                    return Ok(false);
                }
                let asset =
                    saved.asset.clone().ok_or_else(|| Failure::new(400, format!("{prefix}Invalid download url")))?;
                (saved.remote_version.clone(), asset, saved.checksums.clone())
            }
        };
        let entry = &s.entry;
        let gh = self.github.as_ref().map_err(|e| Failure::new(500, e.clone()))?;
        p.send(&format!("{prefix}{}", if rolling_back { "Rolling Back..." } else { "Updating..." }), false).await;
        p.send(&format!("{prefix}Downloading Release..."), false).await;
        let before = Deps::read(&s.entry.kind.deps()).await;
        let new_version = version.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let work = async move {
            let sums = checksums.as_ref();
            match &entry.kind {
                Kind::Archive(_) => {
                    archive::install(gh, entry, &installed, &asset, sums, &version, &p.application, &tx).await
                }
                _ => executable::install(gh, entry, &installed, &asset, sums, &version, &p.application, &tx).await,
            }
        };
        let forward = async {
            while let Some(line) = rx.recv().await {
                p.output(&line).await;
            }
        };
        let (installed_ok, ()) = tokio::join!(work, forward);
        installed_ok.map_err(|e| Failure::new(500, e))?;
        let previous = s.installed.as_ref().expect("just read").version.clone();
        if !rolling_back {
            let old = s.installed.as_ref().expect("just read");
            s.stored.release.rollback_version.clone_from(&old.version);
            s.stored.release.rollback_repo.clone_from(&old.repo);
        }
        self.update_deps(s, &before, false, p).await;
        if rolling_back {
            // A rollback runs neither command, and what failed belonged to the newer version.
            s.stored.failed = None;
        } else {
            let previous = if previous == "?" { String::new() } else { previous };
            self.run_command(s, event, (&new_version, &new_version, &previous), p).await?;
        }
        self.read_repo(s, false).await;
        self.add_pending(&s.entry);
        Ok(true)
    }

    /// Moonraker's `PackageDeploy.update`: every package update from the last check, from
    /// signed sources only. Returns false when the last check found none.
    async fn update_system(&self, s: &mut EntryState, p: &Progress) -> Result<bool, Failure> {
        if s.stored.packages.is_empty() {
            return Ok(false);
        }
        p.send("Updating packages...", false).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let work = async move {
            let pk = PackageKit::connect().await?;
            pk.upgrade(&tx).await
        };
        let forward = async {
            while let Some(line) = rx.recv().await {
                p.output(&line).await;
            }
        };
        let (upgraded, ()) = tokio::join!(work, forward);
        // Moonraker says only "Error updating system packages", without the reason.
        upgraded.map_err(|e| Failure::new(400, format!("Error updating system packages: {e}")))?;
        s.stored.packages.clear();
        self.save(s).await;
        Ok(true)
    }

    /// Moonraker's `NetDeploy.rollback`: the release the last update replaced, downloaded
    /// again. The asset is the one `release_info.json` names; Moonraker takes the release's
    /// first asset.
    async fn rollback_executable(&self, s: &mut EntryState, p: &Progress) -> Result<bool, Failure> {
        self.read_repo(s, false).await;
        let installed = s.installed.clone().expect("just read");
        let saved = s.stored.release.clone();
        // The files the last update replaced, kept in .previous/, need no download. They win
        // over the database's rollback version, since an update from a shell changes them
        // without it.
        let (previous, files) = match &s.entry.kind {
            Kind::Executable(opts) => {
                let files = executable::files(opts, &installed);
                (executable::previous_version(&opts.path, &files), files)
            }
            Kind::Archive(opts) => (archive::previous_version(&opts.path), Vec::new()),
            Kind::Git(_) | Kind::System => unreachable!("a release entry"),
        };
        if let Some(previous) = previous
            && previous != installed.version
        {
            let prefix = s.entry.prefix();
            p.send(&format!("{prefix}Rolling Back..."), false).await;
            p.send(&format!("{prefix}Restoring {previous} from {}...", executable::PREVIOUS), false).await;
            let before = Deps::read(&s.entry.kind.deps()).await;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let entry = &s.entry;
            let work = async move {
                match &entry.kind {
                    Kind::Archive(_) => archive::restore_previous(entry, &tx).await,
                    _ => executable::restore_previous(entry, &files, &tx).await,
                }
            };
            let forward = async {
                while let Some(line) = rx.recv().await {
                    p.output(&line).await;
                }
            };
            let (restored, ()) = tokio::join!(work, forward);
            restored.map_err(|e| Failure::new(500, e))?;
            self.update_deps(s, &before, false, p).await;
            self.read_repo(s, false).await;
            self.add_pending(&s.entry);
            return Ok(true);
        }
        if saved.rollback_version == "?" || saved.rollback_repo == "?" {
            return Err(Failure::new(400, "Incomplete Rollback Data"));
        }
        if saved.rollback_version == installed.version {
            return Ok(false);
        }
        let gh = self.github.as_ref().map_err(|e| Failure::new(500, e.clone()))?;
        let release = match gh.release(&saved.rollback_repo, s.entry.channel, Some(&saved.rollback_version), None).await
        {
            Ok(executable::Fetched::Release(r, _)) if r.is_object() => r,
            Ok(_) => return Err(Failure::new(500, "Failed to retrieve release asset data")),
            Err(e) => return Err(Failure::new(500, e)),
        };
        let (_, asset, _) = executable::read_release(&release, installed.asset_name.as_deref());
        let asset = asset.ok_or_else(|| Failure::new(500, "Failed to retrieve release asset data"))?;
        let checksums = executable::checksums_of(&s.entry, &release);
        self.update_executable(s, p, Some((saved.rollback_version, asset, checksums))).await
    }

    /// orrery's own entry: an executable's update or rollback, refused for a development
    /// build. Afterwards, the entry expects the new version once orrery restarts, at the end
    /// of the command. One put in place before, and not running yet, restarts orrery too.
    async fn update_own(&self, s: &mut EntryState, p: &Progress, rollback: bool) -> Result<bool, Failure> {
        if let Some(problem) = release::problem() {
            return Err(Failure::new(400, problem));
        }
        let done =
            if rollback { self.rollback_executable(s, p).await? } else { self.update_executable(s, p, None).await? };
        if done {
            s.stored.release.expected_version = s.installed.as_ref().map(|i| i.version.clone());
            self.save(s).await;
        }
        if done || release::not_restarted(valid_version(s.installed.as_ref())).is_some() {
            self.add_pending(&s.entry);
            return Ok(true);
        }
        Ok(false)
    }

    /// Asks orrery to restart, which stops this agent and starts the new one.
    async fn restart_self(&self, p: &Progress) {
        let Some(agent) = self.agent() else { return };
        p.send("Restarting orrery...", false).await;
        if let Err(e) = agent.call("server.restart", Value::Null).await {
            p.send(&format!("Can't restart orrery: {e}. Restart it with: sudo systemctl restart orrery"), false).await;
        }
    }

    /// Adds the entry's services to the restarts that wait for the end of the updates.
    fn add_pending(&self, entry: &Entry) {
        let links_into_klipper = self.plugin_links.lock().expect("links lock").by_entry.contains_key(&entry.name);
        let mut pending = self.pending.lock().expect("pending lock");
        let mut services = entry.services();
        // A plugin linked into Klipper's folder changes what Klippy loads.
        if links_into_klipper && !services.iter().any(|s| s == "klipper") {
            services.push("klipper".into());
        }
        for service in services {
            if service == "moonraker" {
                tracing::info!("{}: managed_services names moonraker, which orrery doesn't restart", entry.name);
            } else if !pending.contains(&service) {
                pending.push(service);
            }
        }
    }

    /// Restarts the pending services once, unless another command waits to run, or a print
    /// started during the update; then they restart after it.
    async fn restart_pending(self: &Arc<Self>, p: &Progress) {
        let services = {
            let pending = self.pending.lock().expect("pending lock");
            if pending.is_empty() {
                return;
            }
            pending.clone()
        };
        if self.waiting.load(Ordering::SeqCst) > 0 {
            for service in &services {
                p.send(&format!("Service {service} restart postponed..."), false).await;
            }
            return;
        }
        if self.is_printing() {
            p.send(&format!("A print started, so {} restart when it ends.", services.join(", ")), false).await;
            if !self.restart_after_print.swap(true, Ordering::SeqCst) {
                let m = self.clone();
                tokio::spawn(async move {
                    let mut state = m.print_state.subscribe();
                    let _ = state.wait_for(|s| !matches!(s.as_str(), "printing" | "paused")).await;
                    let _entries = m.entries.lock().await;
                    m.restart_after_print.store(false, Ordering::SeqCst);
                    let quiet = Progress { agent: None, application: String::new(), proc_id: 0 };
                    m.restart_services(&quiet).await;
                    if m.restart_orrery.swap(false, Ordering::SeqCst) {
                        m.restart_self(&quiet).await;
                    }
                });
            }
            return;
        }
        self.restart_services(p).await;
    }

    /// Restarts the pending services now.
    async fn restart_services(&self, p: &Progress) {
        let services = std::mem::take(&mut *self.pending.lock().expect("pending lock"));
        let Some(agent) = self.agent() else { return };
        for service in services {
            if service == "orrery" {
                self.restart_orrery.store(true, Ordering::SeqCst);
                continue;
            }
            p.send(&format!("Restarting service {service}..."), false).await;
            let unit = if service == "klipper" { klipper_unit(&agent).await } else { service.clone() };
            if let Err(e) = agent.call("machine.services.restart", json!({"service": unit})).await {
                p.send(&format!("Can't restart {service}: {e}"), false).await;
            }
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
            // Nothing to check yet, such as before Klipper's entry comes: wait for a change.
            let Some(due_at) = due_at else {
                self.wake.notified().await;
                continue;
            };
            let wait = (due_at - now()).max(0.0);
            if wait > 0.0 {
                if first && !global.enable_auto_refresh {
                    return;
                }
                // A new entry, or a print that ends, wakes this early, and the loop looks again.
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs_f64(wait)) => {}
                    () = self.wake.notified() => continue,
                }
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

/// The service Klipper runs in, such as `klipper` or `klipper-1`, as Moonraker reads it from
/// the Klippy connection.
async fn klipper_unit(agent: &Agent) -> String {
    let info = agent.call("machine.system_info", Value::Null).await.unwrap_or_default();
    info["system_info"]["instance_ids"]["klipper"].as_str().unwrap_or("klipper").to_owned()
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

/// The `system` entry's object in `version_info`, as Moonraker's `PackageDeploy` gives it.
fn system_status(packages: &[String]) -> Value {
    json!({"name": "system", "configured_type": "system", "package_count": packages.len(), "package_list": packages})
}

/// An executable's object in `version_info`, with what's particular to orrery's own.
fn own_status(s: &EntryState) -> Value {
    let Some(installed) = &s.installed else { return s.stored.status.clone() };
    let mut status = executable::status(&s.entry, installed, &s.stored.release);
    if release::is_own(&s.entry) {
        release::adjust_status(&mut status, valid_version(Some(installed)));
    }
    status
}

/// The version of an executable entry's install, when its `release_info.json` is valid and
/// names one.
fn valid_version(installed: Option<&executable::Installed>) -> Option<&str> {
    installed.filter(|i| i.valid && i.version != "?").map(|i| i.version.as_str())
}

/// An entry's object in `version_info` before its first check since the agent started: the
/// stored one, or what Moonraker shows for a repo it hasn't read yet.
fn view_of(s: &EntryState) -> Value {
    if s.stored.status.is_object() && s.stored.config_hash == s.entry.hash {
        return s.stored.status.clone();
    }
    if let Kind::System = s.entry.kind {
        return system_status(&s.stored.packages);
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
    fn each_service_restarts_once() {
        let g = config::global(&Map::new(), &mut Vec::new());
        let e = |name: &str, extra: Value| {
            let mut map = json!({"type": "git_repo", "path": "/x", "origin": "o"});
            map.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            config::entry(name, map.as_object().unwrap(), &g).unwrap()
        };
        let m = Manager::new();
        m.add_pending(&e("crowsnest", json!({})));
        m.add_pending(&e("autotune", json!({"managed_services": "klipper"})));
        m.add_pending(&e("filter_monitor", json!({"managed_services": "klipper moonraker"})));
        m.add_pending(&e("led_effect", json!({"is_system_service": false})));
        assert_eq!(*m.pending.lock().unwrap(), ["crowsnest", "klipper"]);
    }

    #[test]
    fn anomalies_are_logged_once() {
        let a = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
        assert_eq!(new_anomalies(&[], &a(&["x", "y"])), ["x", "y"]);
        assert!(new_anomalies(&a(&["x", "y"]), &a(&["y", "x"])).is_empty());
        assert_eq!(new_anomalies(&a(&["x"]), &a(&["y"])), ["y"]);
    }

    #[tokio::test]
    async fn a_failed_preflight_undoes_a_git_update() {
        let r = git::tests::Repos::new();
        let (mut entry, _) = r.entry("dev");
        entry.preflight = command::words(&["check.sh"]);
        let head = || {
            let out =
                std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(r.path("clone")).output();
            String::from_utf8(out.unwrap().stdout).unwrap().trim().to_owned()
        };
        let start = head();
        // Upstream adds a check that fails.
        let check = |body: &str| {
            r.sh(&format!(
                "cd work && printf '#!/bin/sh\\n{body}\\n' > check.sh && chmod +x check.sh && git add check.sh && git commit -q -m check && git push -q origin main"
            ));
        };
        check("echo not ready; exit 1");
        let m = Manager::new();
        let mut s = EntryState { entry, stored: Stored::default(), repo: None, installed: None };
        let p = Progress { agent: None, application: "test".into(), proc_id: 1 };
        let err = m.update_entry(&mut s, &p).await.unwrap_err();
        assert_eq!(
            err.error,
            "The preflight check `check.sh` failed with exit status 1:\nnot ready\ntest is back at v1.0.0-0. Python packages that the update installed stay installed."
        );
        assert_eq!(head(), start);
        assert!(s.stored.rollback.is_none() && m.pending.lock().unwrap().is_empty());

        // Once the check passes, the update takes effect.
        check("exit 0");
        s.entry.managed_services = Some(vec!["test".into()]);
        assert!(m.update_entry(&mut s, &p).await.unwrap());
        assert_ne!(head(), start);
        assert_eq!(s.stored.rollback.as_ref().unwrap().commit, start);
        assert_eq!(*m.pending.lock().unwrap(), ["test"]);
    }

    #[tokio::test]
    async fn installs_an_executable_and_runs_its_install_again() {
        use std::collections::HashMap;
        let base = executable::tests::server(|base| {
            let release = json!({"tag_name": "v1.2.0", "assets": [
                {"name": "tool-linux", "browser_download_url": format!("{base}dl/tool"), "content_type": "application/octet-stream", "size": 3}]});
            HashMap::from([
                ("/repos/example/tool/releases/latest".to_owned(), release.to_string().into_bytes()),
                ("/dl/tool".to_owned(), b"new".to_vec()),
            ])
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        let g = config::global(&Map::new(), &mut Vec::new());
        let map = json!({"type": "executable", "path": path, "repo": "example/tool", "asset": "tool-linux",
            "install": "test -f ../ready && echo \"$ORRERY_ENTRY $ORRERY_VERSION [$ORRERY_PREVIOUS_VERSION]\" > ran",
            "managed_services": ["tool"]});
        let map = map.as_object().unwrap();
        let mut entry = config::entry("tool", map, &g).unwrap();
        let keys: Vec<String> = map.keys().cloned().collect();
        config::own_options("update_manager tool", map, &keys, &mut entry, &mut Vec::new()).unwrap();
        let m = Manager::with_github(Github::with_api(&base));
        let mut s = EntryState { entry, stored: Stored::default(), repo: None, installed: None };
        let p = Progress { agent: None, application: "test".into(), proc_id: 1 };

        // A missing folder shows v0.0.0-missing, and the update installs the release.
        m.read_repo(&mut s, true).await;
        assert_eq!(s.stored.status["version"], executable::MISSING_VERSION);
        assert_eq!(s.stored.release.remote_version, "v1.2.0");
        let err = m.update_entry(&mut s, &p).await.unwrap_err();
        assert!(err.error.starts_with("The install command `test -f ../ready"), "{}", err.error);
        assert_eq!(std::fs::read(path.join("tool-linux")).unwrap(), b"new");
        assert!(path.join("release_info.json").is_file() && !path.join("ran").exists());
        assert!(!path.join(executable::PREVIOUS).exists());
        assert_eq!(s.stored.failed.as_ref().map(|f| f.event), Some(Event::Install));
        assert_eq!(s.stored.status["version"], executable::UNKNOWN_VERSION);
        assert!(m.pending.lock().unwrap().is_empty());

        // The next update runs only the install command.
        std::fs::write(dir.path().join("ready"), "").unwrap();
        assert!(m.update_entry(&mut s, &p).await.unwrap());
        assert_eq!(std::fs::read_to_string(path.join("ran")).unwrap(), "tool v1.2.0 []\n");
        assert!(s.stored.failed.is_none());
        assert_eq!(s.stored.status["version"], "v1.2.0");
        assert_eq!(*m.pending.lock().unwrap(), ["tool"]);
        // Up to date, nothing runs.
        assert!(!m.update_entry(&mut s, &p).await.unwrap());
    }

    #[tokio::test]
    async fn clones_a_missing_repo_with_install() {
        let r = git::tests::Repos::new();
        let (mut entry, _) = r.entry("dev");
        let path = r.path("apps/tool");
        if let Kind::Git(o) = &mut entry.kind {
            o.path = path.clone();
        }
        entry.managed_services = Some(vec!["test".into()]);
        let m = Manager::new();
        let p = Progress { agent: None, application: "test".into(), proc_id: 1 };

        // Without install, a missing repo is invalid, as in Moonraker.
        let mut s = EntryState { entry: entry.clone(), stored: Stored::default(), repo: None, installed: None };
        m.read_repo(&mut s, true).await;
        assert_eq!(
            (s.stored.status["is_valid"].as_bool(), s.stored.status["version"].as_str()),
            (Some(false), Some("?"))
        );

        // With it, it's valid, shows v0.0.0-missing with the remote's commit, and the update
        // clones it and runs install there.
        entry.install = Some(command::Install::Run(Command::Words(vec![
            "sh".into(),
            "-c".into(),
            "echo $ORRERY_VERSION > installed".into(),
        ])));
        let mut s = EntryState { entry, stored: Stored::default(), repo: None, installed: None };
        m.read_repo(&mut s, true).await;
        let status = &s.stored.status;
        assert_eq!(
            (status["is_valid"].as_bool(), status["version"].as_str()),
            (Some(true), Some(executable::MISSING_VERSION))
        );
        assert_eq!(status["current_hash"], "?");
        assert_eq!(status["remote_hash"].as_str().map(str::len), Some(40));
        assert_eq!(status["remote_version"], "v1.0.0-0");
        assert_eq!(git::summary(s.repo.as_ref().unwrap()), "not installed, v1.0.0-0 available");
        assert!(m.update_entry(&mut s, &p).await.unwrap());
        assert!(path.join(".git").is_dir());
        assert_eq!(std::fs::read_to_string(path.join("installed")).unwrap(), "v1.0.0-0\n");
        assert_eq!(s.stored.status["version"], "v1.0.0-0");
        assert_eq!(*m.pending.lock().unwrap(), ["test"]);
        // Then it's an ordinary repo, up to date.
        assert!(!m.update_entry(&mut s, &p).await.unwrap());

        // A hard recover clones it again, and runs install again.
        std::fs::remove_file(path.join("installed")).unwrap();
        m.recover_entry(&mut s, true, false, &p).await.unwrap();
        assert!(path.join("installed").is_file());
    }

    #[tokio::test]
    async fn a_failed_upgrade_keeps_the_update_and_runs_again() {
        let r = git::tests::Repos::new();
        let (mut entry, _) = r.entry("dev");
        entry.upgrade = Some(Command::Shell("echo \"$ORRERY_PREVIOUS_VERSION\" > seen; test -f ready".into()));
        entry.managed_services = Some(vec!["test".into()]);
        let head = || {
            let out =
                std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(r.path("clone")).output();
            String::from_utf8(out.unwrap().stdout).unwrap().trim().to_owned()
        };
        let start = head();
        r.sh("cd work && echo new > new.txt && git add new.txt && git commit -q -m new && git push -q origin main");
        let m = Manager::new();
        let mut s = EntryState { entry, stored: Stored::default(), repo: None, installed: None };
        let p = Progress { agent: None, application: "test".into(), proc_id: 1 };

        // The upgrade fails: the pull stays, nothing restarts, and the status says what failed.
        let err = m.update_entry(&mut s, &p).await.unwrap_err();
        assert_eq!(
            err.error,
            "The upgrade command `echo \"$ORRERY_PREVIOUS_VERSION\" > seen; test -f ready` failed with exit status 1.\nThe new files stay in place. Update again to run it again."
        );
        assert_ne!(head(), start);
        assert_eq!(std::fs::read_to_string(r.path("clone").join("seen")).unwrap(), "v1.0.0-0\n");
        assert!(m.pending.lock().unwrap().is_empty());
        let failed = s.stored.failed.clone().unwrap();
        assert_eq!((failed.event, failed.at.as_str()), (Event::Upgrade, head().as_str()));
        assert_eq!(s.stored.status["version"], executable::UNKNOWN_VERSION);
        assert_eq!(s.stored.status["is_valid"], true);
        let warnings = s.stored.status["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap().ends_with("failed with exit status 1. Update again to run it again.")),
            "{warnings:?}"
        );
        // The database keeps it.
        assert_eq!(Stored::from_json(&s.stored.to_json()).failed, Some(failed));

        // Fixed, the next update runs only the command, and then the restart.
        std::fs::write(r.path("clone").join("ready"), "").unwrap();
        let moved = head();
        assert!(m.update_entry(&mut s, &p).await.unwrap());
        assert_eq!(head(), moved);
        assert!(s.stored.failed.is_none());
        assert_ne!(s.stored.status["version"], executable::UNKNOWN_VERSION);
        assert_eq!(*m.pending.lock().unwrap(), ["test"]);
    }

    #[test]
    fn plugins_linked_into_klipper_restart_it() {
        let g = config::global(&Map::new(), &mut Vec::new());
        let map = json!({"type": "git_repo", "path": "/x", "origin": "o", "is_system_service": false});
        let led = config::entry("led_effect", map.as_object().unwrap(), &g).unwrap();
        let m = Manager::new();
        m.add_pending(&led);
        assert!(m.pending.lock().unwrap().is_empty());
        m.plugin_links.lock().unwrap().by_entry.insert("led_effect".into(), vec!["klippy/extras/led_effect.py".into()]);
        m.add_pending(&led);
        assert_eq!(*m.pending.lock().unwrap(), ["klipper"]);
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
            rollback: Some(Rollback { commit: "abc".into(), branch: "main".into(), version: "v0-9".into() }),
            release: Saved {
                remote_version: "v1.2.0".into(),
                asset: Some(executable::Asset {
                    url: "https://x/a".into(),
                    content_type: "application/octet-stream".into(),
                    size: 3,
                }),
                prerelease: false,
                etag: Some(("https://api/x".into(), "\"abc\"".into())),
                last_error: String::new(),
                rollback_version: "v1.1.0".into(),
                rollback_repo: "ressu/printerkeep".into(),
                checksums: Some(executable::Asset { url: "https://x/s".into(), content_type: "?".into(), size: 0 }),
                expected_version: Some("v0.2.0".into()),
            },
            packages: vec!["libc6".into()],
            failed: Some(Failed {
                event: Event::Install,
                at: "v1.2.0".into(),
                version: "v1.2.0".into(),
                previous: String::new(),
                error: "The install command `./install.sh` failed with exit status 1.".into(),
            }),
        };
        assert_eq!(Stored::from_json(&s.to_json()), s);
    }
}
