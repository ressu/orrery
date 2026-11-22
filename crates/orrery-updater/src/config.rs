//! The update manager's config: `[update_manager]` and its `[update_manager <name>]` entries,
//! as orrery's `orrery.agent.config` returns them. Values from `orrery.toml` keep their types;
//! values from `moonraker.conf` are text, so every getter takes both.
//!
//! An entry that can't be used is left out, with a warning for `server.info`. The other
//! entries keep working.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::deps::DepFiles;

/// The section, and the agent's name.
pub const SECTION: &str = "update_manager";
/// Names Moonraker gives its own entries, which a configured entry can't take. `klipper` and
/// `kalico` sections hold options for Klipper's entry instead ([`is_klipper_section`]).
const RESERVED: &[&str] = &["moonraker", "system"];
/// The options Klipper's entry takes from its section: Moonraker's `OPTION_OVERRIDES`, and
/// `enabled`.
const KLIPPER_OPTIONS: &[&str] = &["channel", "pinned_commit", "refresh_interval", "report_anomalies", "enabled"];
/// The options that name where Klipper's code comes from. Moonraker fixes them; orrery takes
/// them only from `orrery.toml`, so a config written for Moonraker can't change them.
const KLIPPER_SOURCE: &[&str] = &["origin", "primary_branch", "moved_origin"];
/// Moonraker's default `refresh_interval`, in hours: a weekly check.
const DEFAULT_INTERVAL_HOURS: u64 = 168;

/// A release channel. `dev` follows the branch; `stable` and `beta` follow its tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
    Dev,
}

impl Channel {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Self::Stable),
            "beta" => Some(Self::Beta),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Dev => "dev",
        }
    }
}

/// `[update_manager]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Global {
    pub refresh_interval: Duration,
    /// Periodic checks; on by default (docs/design/2026-11-22-update-manager.md, "Refreshing").
    pub enable_auto_refresh: bool,
    /// Local hours `(start, end)`: checks wait for `start <= hour < end`, or across midnight
    /// when `start > end`. None by default.
    pub refresh_window: Option<(u32, u32)>,
    pub channel: Option<Channel>,
    /// The `system` entry, for Debian's packages; on by default, as in Moonraker.
    pub enable_system_updates: bool,
}

/// One `[update_manager <name>]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub channel: Channel,
    /// Set when the entry asked for a channel its type doesn't have.
    pub channel_warning: Option<String>,
    pub refresh_interval: Duration,
    /// As written; `None` when the option isn't there. [`Entry::services`] reads it.
    pub managed_services: Option<Vec<String>>,
    pub is_system_service: bool,
    pub info_tags: Vec<String>,
    pub report_anomalies: bool,
    /// A hash of the entry's options: a changed entry is checked again at once.
    pub hash: String,
    pub kind: Kind,
    /// For Klipper's own entry: the project it runs.
    pub klippy: Option<Project>,
    /// The check that the new version can run, as words; empty for none ([`preflight`]).
    pub preflight: Vec<String>,
}

/// The projects Klipper's entry knows (docs/design/2026-11-22-klipper-updates.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Project {
    Klipper,
    Kalico,
}

impl Project {
    /// The project's name in messages.
    pub fn title(self) -> &'static str {
        match self {
            Self::Klipper => "Klipper",
            Self::Kalico => "Kalico",
        }
    }

    /// The entry's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Klipper => "klipper",
            Self::Kalico => "kalico",
        }
    }

    pub fn origin(self) -> &'static str {
        match self {
            Self::Klipper => "https://github.com/Klipper3d/klipper.git",
            Self::Kalico => "https://github.com/KalicoCrew/kalico.git",
        }
    }

    pub fn primary_branch(self) -> &'static str {
        match self {
            Self::Klipper => "master",
            Self::Kalico => "main",
        }
    }

    pub fn moved_origin(self) -> &'static str {
        match self {
            Self::Klipper => "https://github.com/kevinoconnor/klipper.git",
            Self::Kalico => "https://github.com/DangerKlippers/danger-klipper.git",
        }
    }

    /// From `printer.info`'s `app`, which only Kalico sends, then from the repo's remote.
    /// Anything else is Klipper, as Moonraker assumes.
    pub fn detect(app: Option<&str>, remote: Option<&str>) -> Self {
        let kalico = Self::Kalico;
        let app_says = app.is_some_and(|a| matches!(a.to_ascii_lowercase().as_str(), "kalico" | "danger-klipper"));
        let remote_says = remote.is_some_and(|r| {
            crate::git::same_url(r, kalico.origin()) || crate::git::same_url(r, kalico.moved_origin())
        });
        if app_says || remote_says { kalico } else { Self::Klipper }
    }
}

/// What Klippy reports about itself in `printer.info`.
#[derive(Debug, Clone, PartialEq)]
pub struct Klippy {
    pub project: Project,
    /// `klipper_path`: the repo.
    pub path: PathBuf,
    /// `python_path`: the Python that runs Klippy.
    pub python: PathBuf,
}

/// A `klipper` or `kalico` section, with the options that came from `orrery.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct KlipperSection {
    pub name: String,
    pub options: Map<String, Value>,
    pub from_orrery_toml: Vec<String>,
}

/// An entry's `preflight`: a command as a list of words. It's orrery's own option, and runs a
/// command, so it's taken only from `orrery.toml`; in `moonraker.conf` it's ignored, with a
/// warning. For an executable, the words are arguments to the new program; for a `git_repo`,
/// the first word is a file in the repo.
pub fn preflight(
    section: &str,
    map: &Map<String, Value>,
    from_orrery_toml: &[String],
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, String> {
    let Some(value) = map.get("preflight") else { return Ok(Vec::new()) };
    if !from_orrery_toml.iter().any(|k| k == "preflight") {
        warnings.push(ignored_options(section, &["preflight"], "it takes it only from orrery.toml"));
        return Ok(Vec::new());
    }
    let words = value
        .as_array()
        .and_then(|items| items.iter().map(|i| i.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>())
        .filter(|w| !w.is_empty())
        .ok_or_else(|| {
            format!("[{section}] preflight: expected a list of words, such as [\"--check\"], got {value}")
        })?;
    Ok(words)
}

/// "[update_manager klipper]: orrery ignores the option path, since …"
fn ignored_options(section: &str, keys: &[&str], since: &str) -> String {
    let what = if keys.len() == 1 { "option" } else { "options" };
    format!("[{section}]: orrery ignores the {what} {}, since {since}", keys.join(", "))
}

/// Whether a section holds options for Klipper's entry.
pub fn is_klipper_section(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "klipper" | "kalico")
}

/// Klipper's entry, from Klippy's paths and Moonraker's defaults, with the options of its
/// section: `klipper` or `kalico`, whichever is there. Kalico's docs tell users to write
/// `klipper`. Both at once is an error, since neither one is clearly meant. `None` when the
/// section sets `enabled = false`. `origin`, `primary_branch`, and `moved_origin` name another
/// source, from `orrery.toml` only. Other options Moonraker doesn't allow for this entry are
/// ignored, with a warning.
pub fn klipper_entry(
    k: &Klippy,
    sections: &[KlipperSection],
    global: &Global,
    warnings: &mut Vec<String>,
) -> Result<Option<Entry>, String> {
    let name = k.project.name();
    if sections.len() > 1 {
        let names: Vec<String> = sections.iter().map(|s| format!("[{SECTION} {}]", s.name)).collect();
        return Err(format!(
            "{} are both there, so orrery leaves out the entry for {name}. Keep only one of them.",
            names.join(" and ")
        ));
    }
    let chosen = sections.first();
    let empty = Map::new();
    let no_toml = Vec::new();
    let (section, map, from_toml) =
        chosen.map_or((name, &empty, &no_toml), |s| (s.name.as_str(), &s.options, &s.from_orrery_toml));
    let o = Options { section: format!("{SECTION} {section}"), map };
    if !o.bool("enabled", true)? {
        return Ok(None);
    }
    let ignored: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !KLIPPER_OPTIONS.contains(k) && !KLIPPER_SOURCE.contains(k) && *k != "preflight")
        .collect();
    if !ignored.is_empty() {
        let them = if ignored.len() == 1 { "it" } else { "them" };
        warnings.push(ignored_options(&o.section, &ignored, &format!("it sets {them} for {name} itself")));
    }
    let not_toml: Vec<&str> =
        KLIPPER_SOURCE.iter().copied().filter(|k| map.contains_key(*k) && !from_toml.iter().any(|t| t == k)).collect();
    if !not_toml.is_empty() {
        let them = if not_toml.len() == 1 { "it" } else { "them" };
        warnings.push(ignored_options(&o.section, &not_toml, &format!("it takes {them} only from orrery.toml")));
    }
    // Another source, which orrery.toml names on purpose.
    let source = |key: &str| -> Result<Option<String>, String> {
        if from_toml.iter().any(|t| t == key) { o.text(key) } else { Ok(None) }
    };
    let preflight = preflight(&o.section, map, from_toml, warnings)?;
    // Two folders above Klippy's Python is its virtualenv, as for Moonraker's `env` option.
    let virtualenv =
        k.python.parent().and_then(Path::parent).filter(|v| v.join("bin/activate").is_file()).map(Path::to_path_buf);
    let mut hashed = map.clone();
    hashed.insert("klippy".into(), serde_json::json!([name, k.path, k.python]));
    Ok(Some(Entry {
        name: name.to_owned(),
        channel: o.channel()?.or(global.channel).unwrap_or(Channel::Dev),
        channel_warning: None,
        refresh_interval: o.hours("refresh_interval")?.unwrap_or(global.refresh_interval),
        managed_services: Some(vec!["klipper".into()]),
        is_system_service: false,
        info_tags: Vec::new(),
        report_anomalies: o.bool("report_anomalies", true)?,
        hash: hash(&hashed),
        kind: Kind::Git(GitOptions {
            path: k.path.clone(),
            origin: source("origin")?.unwrap_or_else(|| k.project.origin().into()),
            moved_origin: Some(source("moved_origin")?.unwrap_or_else(|| k.project.moved_origin().into())),
            primary_branch: source("primary_branch")?.unwrap_or_else(|| k.project.primary_branch().into()),
            pinned_commit: o.text("pinned_commit")?.map(|c| c.to_ascii_lowercase()),
            virtualenv,
            pip_env: Vec::new(),
            requirements: Some("scripts/klippy-requirements.txt".into()),
            system_dependencies: None,
            install_script: Some("scripts/install-octopi.sh".into()),
        }),
        klippy: Some(k.project),
        preflight,
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Git(GitOptions),
    Executable(ExecOptions),
    /// Moonraker's `system` entry: the system's packages, through PackageKit.
    System,
}

impl Kind {
    pub fn configured_type(&self) -> &'static str {
        match self {
            Self::Git(_) => "git_repo",
            Self::Executable(_) => "executable",
            Self::System => "system",
        }
    }

    /// Where the entry lists its dependencies. An executable has only system packages, as in
    /// Moonraker.
    pub fn deps(&self) -> DepFiles {
        match self {
            Self::Git(g) => DepFiles {
                path: g.path.clone(),
                virtualenv: g.virtualenv.clone(),
                pip_env: g.pip_env.clone(),
                requirements: g.requirements.clone(),
                system_dependencies: g.system_dependencies.clone(),
                install_script: g.install_script.clone(),
            },
            Self::Executable(e) => DepFiles {
                path: e.path.clone(),
                system_dependencies: e.system_dependencies.clone(),
                ..DepFiles::default()
            },
            Self::System => DepFiles::default(),
        }
    }
}

/// An `executable` entry: a program from a project's GitHub releases.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecOptions {
    /// The folder with the program and its `release_info.json`.
    pub path: PathBuf,
    /// `owner/name` on GitHub.
    pub repo: String,
    pub system_dependencies: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitOptions {
    pub path: PathBuf,
    pub origin: String,
    pub moved_origin: Option<String>,
    pub primary_branch: String,
    pub pinned_commit: Option<String>,
    /// The virtualenv folder, from `virtualenv`, or from `env`, which names its Python.
    pub virtualenv: Option<PathBuf>,
    /// `pip_environment_variables`: set for pip.
    pub pip_env: Vec<(String, String)>,
    pub requirements: Option<String>,
    pub system_dependencies: Option<String>,
    pub install_script: Option<String>,
}

/// A section's options, from either file.
struct Options<'a> {
    section: String,
    map: &'a Map<String, Value>,
}

impl Options<'_> {
    fn text(&self, key: &str) -> Result<Option<String>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_owned())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(other) => Err(format!("[{}] {key}: expected text, got {other}", self.section)),
        }
    }

    fn required(&self, key: &str) -> Result<String, String> {
        self.text(key)?.filter(|s| !s.is_empty()).ok_or_else(|| format!("[{}] needs the {key} option", self.section))
    }

    /// Moonraker's booleans: `true`, `yes`, `on`, and `1`, or their opposites, in any case.
    fn bool(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Ok(true),
                "false" | "no" | "off" | "0" => Ok(false),
                _ => Err(format!("[{}] {key}: expected true or false, got {s:?}", self.section)),
            },
            Some(other) => Err(format!("[{}] {key}: expected true or false, got {other}", self.section)),
        }
    }

    fn hours(&self, key: &str) -> Result<Option<Duration>, String> {
        let hours = match self.map.get(key) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            Some(_) => None,
        };
        match hours {
            Some(h) if h >= 1 => Ok(Some(Duration::from_secs(h * 3600))),
            _ => Err(format!(
                "[{}] {key}: expected a whole number of hours, at least 1, got {}",
                self.section, self.map[key]
            )),
        }
    }

    /// A list: a TOML array, or `moonraker.conf` text split at spaces, commas, and newlines.
    fn list(&self, key: &str) -> Result<Vec<String>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(s
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|w| !w.is_empty())
                .map(str::to_owned)
                .collect()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|i| i.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("[{}] {key}: expected a list of names", self.section)),
            Some(other) => Err(format!("[{}] {key}: expected a list of names, got {other}", self.section)),
        }
    }

    /// `KEY=value` lines in `moonraker.conf`, or a TOML table.
    fn pairs(&self, key: &str) -> Result<Vec<(String, String)>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => s
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    l.split_once('=')
                        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
                        .ok_or_else(|| format!("[{}] {key}: expected KEY=value, got {l:?}", self.section))
                })
                .collect(),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| match v {
                    Value::String(v) => Ok((k.clone(), v.clone())),
                    other => Ok((k.clone(), other.to_string())),
                })
                .collect(),
            Some(other) => Err(format!("[{}] {key}: expected KEY=value lines, got {other}", self.section)),
        }
    }

    fn channel(&self) -> Result<Option<Channel>, String> {
        match self.text("channel")? {
            None => Ok(None),
            Some(c) => Channel::parse(&c)
                .map(Some)
                .ok_or_else(|| format!("[{}] channel: expected stable, beta, or dev, got {c:?}", self.section)),
        }
    }
}

/// `~/` at the start of a path is the home folder, as in Moonraker.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None if path == "~" => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
        None => PathBuf::from(path),
    }
}

/// The virtualenv folder: `virtualenv`, relative to the repo, or two folders above the
/// Python that the older `env` option names, as Moonraker reads them.
fn virtualenv(o: &Options) -> Result<Option<PathBuf>, String> {
    if let Some(v) = o.text("virtualenv")? {
        let v = expand_home(&v);
        if v.is_absolute() {
            return Ok(Some(v));
        }
        return Ok(o.text("path")?.map(|p| expand_home(&p).join(v)));
    }
    Ok(o.text("env")?.map(|e| {
        let python = expand_home(&e);
        python.parent().and_then(Path::parent).map_or(python.clone(), Path::to_path_buf)
    }))
}

/// Reads `[update_manager]`. A bad option is a warning, and its default applies.
pub fn global(map: &Map<String, Value>, warnings: &mut Vec<String>) -> Global {
    let o = Options { section: SECTION.into(), map };
    let refresh_interval = or_warn(o.hours("refresh_interval"), None, warnings)
        .unwrap_or(Duration::from_secs(DEFAULT_INTERVAL_HOURS * 3600));
    let enable_auto_refresh = or_warn(o.bool("enable_auto_refresh", true), true, warnings);
    let window = o.text("refresh_window").and_then(|w| w.map(|w| parse_window(&w)).transpose());
    let refresh_window = or_warn(window, None, warnings);
    let channel = or_warn(o.channel(), None, warnings);
    let enable_system_updates = or_warn(o.bool("enable_system_updates", true), true, warnings);
    Global { refresh_interval, enable_auto_refresh, refresh_window, channel, enable_system_updates }
}

/// The value, or the default and a warning.
fn or_warn<T>(r: Result<T, String>, default: T, warnings: &mut Vec<String>) -> T {
    r.unwrap_or_else(|e| {
        warnings.push(e);
        default
    })
}

/// `0-5`: from midnight up to 5 am, local time.
fn parse_window(text: &str) -> Result<(u32, u32), String> {
    let bad = || format!("[{SECTION}] refresh_window: expected two hours, such as 0-5, got {text:?}");
    let (start, end) = text.split_once('-').ok_or_else(bad)?;
    let (start, end): (u32, u32) = (start.trim().parse().map_err(|_| bad())?, end.trim().parse().map_err(|_| bad())?);
    if start > 23 || end > 23 || start == end {
        return Err(bad());
    }
    Ok((start, end))
}

/// Moonraker's `system` entry, checked as often as the others. It has no options of its own.
pub fn system_entry(global: &Global) -> Entry {
    Entry {
        name: "system".into(),
        channel: Channel::Stable,
        channel_warning: None,
        refresh_interval: global.refresh_interval,
        managed_services: Some(Vec::new()),
        is_system_service: false,
        info_tags: Vec::new(),
        report_anomalies: true,
        hash: "system".into(),
        kind: Kind::System,
        klippy: None,
        preflight: Vec::new(),
    }
}

/// Reads one entry. `Err` is the warning that says why it's left out.
pub fn entry(name: &str, map: &Map<String, Value>, global: &Global) -> Result<Entry, String> {
    let o = Options { section: format!("{SECTION} {name}"), map };
    if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("[{}]: {name} is a name Moonraker uses for its own entry; choose another", o.section));
    }
    let kind = match o.text("type")?.as_deref() {
        Some("git_repo") => Kind::Git(GitOptions {
            path: expand_home(&o.required("path")?),
            origin: o.required("origin")?,
            moved_origin: o.text("moved_origin")?,
            primary_branch: o.text("primary_branch")?.unwrap_or_else(|| "master".into()),
            pinned_commit: o.text("pinned_commit")?.map(|c| c.to_ascii_lowercase()),
            virtualenv: virtualenv(&o)?,
            pip_env: o.pairs("pip_environment_variables")?,
            requirements: o.text("requirements")?,
            system_dependencies: o.text("system_dependencies")?,
            install_script: o.text("install_script")?,
        }),
        Some("executable") => Kind::Executable(ExecOptions {
            path: expand_home(&o.required("path")?),
            repo: {
                let repo = o.required("repo")?.trim().trim_matches('/').to_owned();
                if repo.split('/').filter(|p| !p.is_empty()).count() != 2 {
                    return Err(format!("[{}] repo: expected owner/name, got {repo:?}", o.section));
                }
                repo
            },
            system_dependencies: o.text("system_dependencies")?,
        }),
        Some(t @ ("web" | "zip" | "python")) => {
            return Err(format!("[{}]: orrery doesn't update {t} entries yet, so this entry is left out", o.section));
        }
        Some(t) => return Err(format!("[{}] type: orrery doesn't know the type {t:?}", o.section)),
        None => return Err(format!("[{}] needs a type option, such as git_repo", o.section)),
    };
    let (channel, channel_warning) = match &kind {
        Kind::Git(_) | Kind::System => (o.channel()?.or(global.channel).unwrap_or(Channel::Dev), None),
        // Releases have no dev channel. Moonraker warns about one in the entry, and falls back
        // to stable.
        Kind::Executable(_) => match o.channel()? {
            Some(Channel::Dev) => (
                Channel::Stable,
                Some(format!(
                    "[{}]: Invalid value 'dev' for option 'channel'. Type 'executable' supports the following channels: ['stable', 'beta'].  Falling back to channel 'stable'",
                    o.section
                )),
            ),
            Some(c) => (c, None),
            None => (global.channel.filter(|c| *c != Channel::Dev).unwrap_or(Channel::Stable), None),
        },
    };
    Ok(Entry {
        name: name.to_owned(),
        channel,
        channel_warning,
        refresh_interval: o.hours("refresh_interval")?.unwrap_or(global.refresh_interval),
        managed_services: o.map.contains_key("managed_services").then(|| o.list("managed_services")).transpose()?,
        is_system_service: o.bool("is_system_service", true)?,
        info_tags: o.list("info_tags")?,
        report_anomalies: o.bool("report_anomalies", true)?,
        hash: hash(map),
        kind,
        klippy: None,
        preflight: Vec::new(),
    })
}

impl Entry {
    /// What Moonraker puts before the entry's progress messages.
    pub fn prefix(&self) -> String {
        match self.kind {
            Kind::Git(_) => format!("Git Repo {}: ", self.name),
            Kind::Executable(_) => format!("Executable {}: ", self.name),
            Kind::System => "PackageKit: ".into(),
        }
    }

    /// The message that ends a successful update of this entry.
    pub fn finished(&self) -> String {
        match self.kind {
            Kind::System => "Package update finished...".into(),
            _ => format!("{}Update Finished...", self.prefix()),
        }
    }

    /// The services an update restarts, as Moonraker's `_configure_managed_services` picks
    /// them: by default the entry's own service, unless `is_system_service` is false. Only
    /// the entry's name, `klipper`, and `moonraker` are allowed, in that order.
    pub fn services(&self) -> Vec<String> {
        let listed = match &self.managed_services {
            Some(l) => l.clone(),
            None if self.is_system_service => vec![self.name.clone()],
            None => Vec::new(),
        };
        [self.name.as_str(), "klipper", "moonraker"]
            .into_iter()
            .filter(|c| listed.iter().any(|l| l == c))
            .map(str::to_owned)
            .collect()
    }

    /// Moonraker's warning for a `managed_services` value it doesn't allow.
    pub fn services_warning(&self) -> Option<String> {
        let listed = self.managed_services.as_ref()?;
        let bad = listed.iter().find(|l| ![self.name.as_str(), "klipper", "moonraker"].contains(&l.as_str()))?;
        Some(format!(
            "[update_manager {}]: Option 'managed_services: {}' contains an invalid value '{bad}'. All values must be one of the following choices: {}, klipper, moonraker",
            self.name,
            listed.join(" "),
            self.name
        ))
    }
}

/// FNV-1a over the options in key order: stable across builds, unlike `std`'s hasher.
fn hash(map: &Map<String, Value>) -> String {
    let sorted: BTreeMap<&String, &Value> = map.iter().collect();
    let text = serde_json::to_string(&sorted).expect("JSON serializes");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn global_defaults_and_text_values() {
        let mut w = Vec::new();
        let g = global(&Map::new(), &mut w);
        assert_eq!(g.refresh_interval, Duration::from_secs(168 * 3600));
        assert!(g.enable_auto_refresh && g.refresh_window.is_none() && w.is_empty());

        let g = global(
            &map(json!({"refresh_interval": "24", "enable_auto_refresh": "False", "refresh_window": "22-5"})),
            &mut w,
        );
        assert_eq!(
            (g.refresh_interval, g.enable_auto_refresh, g.refresh_window),
            (Duration::from_secs(86400), false, Some((22, 5)))
        );

        let g = global(&map(json!({"refresh_interval": 0, "refresh_window": "5-5", "channel": "nightly"})), &mut w);
        assert_eq!(g.refresh_interval, Duration::from_secs(168 * 3600));
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("at least 1") && w[1].contains("two hours") && w[2].contains("stable, beta, or dev"));
    }

    #[test]
    fn git_entries() {
        let g = global(&Map::new(), &mut Vec::new());
        // bluev2's filter_monitor, as moonraker.conf text.
        let e = entry(
            "filter_monitor",
            &map(json!({
                "type": "git_repo", "path": "/home/printer/klipper-filter_monitor",
                "origin": "https://github.com/ressu/klipper-filter_monitor.git",
                "managed_services": "klipper moonraker", "primary_branch": "main", "install_script": "install.sh",
            })),
            &g,
        )
        .unwrap();
        assert_eq!(e.services(), ["klipper", "moonraker"]);
        assert_eq!((e.channel, e.is_system_service, e.refresh_interval), (Channel::Dev, true, g.refresh_interval));
        let Kind::Git(git) = &e.kind else { panic!("a git entry") };
        assert_eq!((git.primary_branch.as_str(), git.install_script.as_deref()), ("main", Some("install.sh")));

        // orrery.toml's types.
        let e = entry(
            "led_effect",
            &map(json!({"type": "git_repo", "path": "/x", "origin": "o", "is_system_service": false, "managed_services": ["led_effect", "crowsnest"], "channel": "stable"})),
            &g,
        )
        .unwrap();
        assert_eq!(
            (e.is_system_service, e.services(), e.channel),
            (false, vec!["led_effect".to_owned()], Channel::Stable)
        );
        assert!(e.services_warning().unwrap().contains("an invalid value 'crowsnest'"));
        // Without managed_services, an entry restarts its own service unless it says it has none.
        let own = entry("crowsnest", &map(json!({"type": "git_repo", "path": "/x", "origin": "o"})), &g).unwrap();
        assert_eq!((own.services(), own.services_warning()), (vec!["crowsnest".to_owned()], None));
        let none = entry(
            "x",
            &map(json!({"type": "git_repo", "path": "/x", "origin": "o", "is_system_service": "False"})),
            &g,
        )
        .unwrap();
        assert!(none.services().is_empty());
        let Kind::Git(git) = &e.kind else { panic!("a git entry") };
        assert_eq!(git.primary_branch, "master");

        // `virtualenv` is relative to the repo; the older `env` names the virtualenv's Python.
        let v = |extra: Value| {
            let mut m = json!({"type": "git_repo", "path": "/home/pi/repo", "origin": "o"});
            m.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let e = entry("v", &map(m), &g).unwrap();
            let Kind::Git(git) = e.kind else { panic!("a git entry") };
            git
        };
        assert_eq!(v(json!({"virtualenv": "env"})).virtualenv, Some(PathBuf::from("/home/pi/repo/env")));
        assert_eq!(v(json!({"virtualenv": "/opt/env"})).virtualenv, Some(PathBuf::from("/opt/env")));
        assert_eq!(
            v(json!({"env": "/home/pi/klippy-env/bin/python"})).virtualenv,
            Some(PathBuf::from("/home/pi/klippy-env"))
        );
        let pip = v(json!({"pip_environment_variables": "CFLAGS=-O2\nSKIP=1"})).pip_env;
        assert_eq!(pip, [("CFLAGS".into(), "-O2".into()), ("SKIP".into(), "1".into())]);
        assert_eq!(v(json!({"pip_environment_variables": {"A": "b"}})).pip_env, [("A".into(), "b".into())]);

        // The hash follows the options, not their order.
        let a = entry("a", &map(json!({"type": "git_repo", "path": "/x", "origin": "o"})), &g).unwrap();
        let b = entry("a", &map(json!({"origin": "o", "path": "/x", "type": "git_repo"})), &g).unwrap();
        let c = entry("a", &map(json!({"origin": "o2", "path": "/x", "type": "git_repo"})), &g).unwrap();
        assert!(a.hash == b.hash && a.hash != c.hash);
    }

    #[test]
    fn bad_entries_are_warnings() {
        let g = global(&Map::new(), &mut Vec::new());
        let err = |name: &str, v: Value| entry(name, &map(v), &g).unwrap_err();
        assert!(err("x", json!({"type": "web"})).contains("doesn't update web entries yet"));
        assert!(err("x", json!({"type": "svn"})).contains("doesn't know the type"));
        assert!(err("x", json!({})).contains("needs a type"));
        assert!(err("x", json!({"type": "git_repo", "path": "/x"})).contains("needs the origin option"));
        assert!(err("Moonraker", json!({"type": "git_repo"})).contains("Moonraker uses"));
        assert!(
            err("x", json!({"type": "git_repo", "path": "/x", "origin": "o", "is_system_service": "maybe"}))
                .contains("expected true or false")
        );
    }

    fn section(name: &str, options: Value, from_toml: &[&str]) -> KlipperSection {
        KlipperSection {
            name: name.into(),
            options: map(options),
            from_orrery_toml: from_toml.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    fn klippy(project: Project, dir: &Path) -> Klippy {
        Klippy { project, path: dir.join("klipper"), python: dir.join("klippy-env/bin/python") }
    }

    #[test]
    fn detects_kalico_or_klipper() {
        assert_eq!(Project::detect(Some("Kalico"), None), Project::Kalico);
        // Klipper sends no app name: the remote decides.
        assert_eq!(Project::detect(None, Some("https://github.com/KalicoCrew/kalico")), Project::Kalico);
        assert_eq!(
            Project::detect(None, Some("https://github.com/DangerKlippers/danger-klipper.git")),
            Project::Kalico
        );
        assert_eq!(Project::detect(None, Some("https://github.com/Klipper3d/klipper.git")), Project::Klipper);
        assert_eq!(Project::detect(None, None), Project::Klipper);
    }

    #[test]
    fn klipper_entry_from_klippy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("klippy-env/bin")).unwrap();
        std::fs::write(dir.path().join("klippy-env/bin/activate"), "").unwrap();
        let g = global(&Map::new(), &mut Vec::new());
        let mut w = Vec::new();
        let e = klipper_entry(&klippy(Project::Kalico, dir.path()), &[], &g, &mut w).unwrap().unwrap();
        assert_eq!((e.name.as_str(), e.channel, e.services()), ("kalico", Channel::Dev, vec!["klipper".to_owned()]));
        assert_eq!(e.klippy, Some(Project::Kalico));
        let Kind::Git(o) = &e.kind else { panic!("a git entry") };
        assert_eq!(
            (o.origin.as_str(), o.primary_branch.as_str()),
            ("https://github.com/KalicoCrew/kalico.git", "main")
        );
        assert_eq!(o.virtualenv.as_deref(), Some(dir.path().join("klippy-env").as_path()));
        assert_eq!(o.requirements.as_deref(), Some("scripts/klippy-requirements.txt"));
        assert!(w.is_empty(), "{w:?}");

        // Kalico's docs say [update_manager klipper]. From moonraker.conf, the source stays
        // Kalico's, and other options Moonraker doesn't allow are ignored too.
        let conf = |opts: Value| vec![section("klipper", opts, &[])];
        let sections = conf(json!({"channel": "stable", "origin": "https://example.com/k", "path": "~/x"}));
        let e = klipper_entry(&klippy(Project::Kalico, dir.path()), &sections, &g, &mut w).unwrap().unwrap();
        assert_eq!(e.channel, Channel::Stable);
        let Kind::Git(o) = &e.kind else { panic!("a git entry") };
        assert_eq!(o.origin, "https://github.com/KalicoCrew/kalico.git");
        assert_eq!(
            w,
            [
                "[update_manager klipper]: orrery ignores the option path, since it sets it for kalico itself",
                "[update_manager klipper]: orrery ignores the option origin, since it takes it only from orrery.toml",
            ]
        );

        // From orrery.toml, a fork on purpose.
        w.clear();
        let fork = json!({"origin": "https://github.com/someone/klipper", "primary_branch": "work"});
        let sections = vec![section("klipper", fork, &["origin", "primary_branch"])];
        let e = klipper_entry(&klippy(Project::Klipper, dir.path()), &sections, &g, &mut w).unwrap().unwrap();
        let Kind::Git(o) = &e.kind else { panic!("a git entry") };
        assert_eq!(
            (o.origin.as_str(), o.primary_branch.as_str(), o.moved_origin.as_deref()),
            ("https://github.com/someone/klipper", "work", Some("https://github.com/kevinoconnor/klipper.git"))
        );
        assert!(w.is_empty(), "{w:?}");

        // A preflight, from orrery.toml only, as for any entry.
        let sections = vec![section("klipper", json!({"preflight": ["scripts/check_config.sh"]}), &["preflight"])];
        let e = klipper_entry(&klippy(Project::Kalico, dir.path()), &sections, &g, &mut w).unwrap().unwrap();
        assert_eq!(e.preflight, ["scripts/check_config.sh"]);
        assert!(w.is_empty(), "{w:?}");
        let sections = vec![section("klipper", json!({"preflight": "scripts/check_config.sh"}), &[])];
        let e = klipper_entry(&klippy(Project::Kalico, dir.path()), &sections, &g, &mut w).unwrap().unwrap();
        assert!(e.preflight.is_empty());
        assert_eq!(
            w,
            ["[update_manager klipper]: orrery ignores the option preflight, since it takes it only from orrery.toml"]
        );

        // Either name works, for either project; both at once is an error.
        let sections = vec![section("kalico", json!({"channel": "beta"}), &["channel"])];
        let e = klipper_entry(&klippy(Project::Kalico, dir.path()), &sections, &g, &mut Vec::new()).unwrap().unwrap();
        assert_eq!(e.channel, Channel::Beta);
        let sections = vec![
            section("klipper", json!({"channel": "stable"}), &[]),
            section("kalico", json!({"channel": "beta"}), &[]),
        ];
        let err = klipper_entry(&klippy(Project::Kalico, dir.path()), &sections, &g, &mut Vec::new()).unwrap_err();
        assert_eq!(
            err,
            "[update_manager klipper] and [update_manager kalico] are both there, so orrery leaves out the entry for kalico. Keep only one of them."
        );

        // Turned off, and a Klipper without a virtualenv.
        let off = vec![section("klipper", json!({"enabled": false}), &[])];
        assert!(klipper_entry(&klippy(Project::Klipper, dir.path()), &off, &g, &mut Vec::new()).unwrap().is_none());
        let other = tempfile::tempdir().unwrap();
        let e = klipper_entry(&klippy(Project::Klipper, other.path()), &[], &g, &mut Vec::new()).unwrap().unwrap();
        let Kind::Git(o) = &e.kind else { panic!("a git entry") };
        assert_eq!((e.name.as_str(), o.primary_branch.as_str(), o.virtualenv.as_ref()), ("klipper", "master", None));
    }

    #[test]
    fn preflight_only_from_orrery_toml() {
        let sec = "update_manager tool";
        let mut w = Vec::new();
        let m = map(json!({"preflight": ["--check", "--quiet"]}));
        assert_eq!(preflight(sec, &m, &["preflight".into()], &mut w).unwrap(), ["--check", "--quiet"]);
        assert!(w.is_empty());
        // From moonraker.conf, it's text, and it's ignored.
        let m = map(json!({"preflight": "--check"}));
        assert!(preflight(sec, &m, &[], &mut w).unwrap().is_empty());
        assert_eq!(
            w,
            ["[update_manager tool]: orrery ignores the option preflight, since it takes it only from orrery.toml"]
        );
        // In orrery.toml, it's a list of words.
        let err = preflight(sec, &m, &["preflight".into()], &mut Vec::new()).unwrap_err();
        assert_eq!(
            err,
            r#"[update_manager tool] preflight: expected a list of words, such as ["--check"], got "--check""#
        );
        assert!(preflight(sec, &map(json!({})), &[], &mut Vec::new()).unwrap().is_empty());
    }
}
