//! The update manager's config: `[update_manager]` and its `[update_manager <name>]` entries,
//! as orrery's `orrery.agent.config` returns them. Values from `orrery.toml` keep their types;
//! values from `moonraker.conf` are text, so every getter takes both.
//!
//! An entry that can't be used is left out, with a warning for `server.info`. The other
//! entries keep working.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value};

/// The section, and the agent's name.
pub const SECTION: &str = "update_manager";
/// Names Moonraker gives its own entries. orrery has none of them yet, and refuses them as
/// entry names so a later built-in entry can't clash.
const RESERVED: &[&str] = &["klipper", "moonraker", "system"];
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
}

/// One `[update_manager <name>]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub channel: Channel,
    pub refresh_interval: Duration,
    /// As written; `None` when the option isn't there. [`Entry::services`] reads it.
    pub managed_services: Option<Vec<String>>,
    pub is_system_service: bool,
    pub info_tags: Vec<String>,
    pub report_anomalies: bool,
    /// A hash of the entry's options: a changed entry is checked again at once.
    pub hash: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Git(GitOptions),
}

impl Kind {
    pub fn configured_type(&self) -> &'static str {
        match self {
            Self::Git(_) => "git_repo",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitOptions {
    pub path: PathBuf,
    pub origin: String,
    pub moved_origin: Option<String>,
    pub primary_branch: String,
    pub pinned_commit: Option<String>,
    pub virtualenv: Option<PathBuf>,
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

/// Reads `[update_manager]`. A bad option is a warning, and its default applies.
pub fn global(map: &Map<String, Value>, warnings: &mut Vec<String>) -> Global {
    let o = Options { section: SECTION.into(), map };
    let refresh_interval = or_warn(o.hours("refresh_interval"), None, warnings)
        .unwrap_or(Duration::from_secs(DEFAULT_INTERVAL_HOURS * 3600));
    let enable_auto_refresh = or_warn(o.bool("enable_auto_refresh", true), true, warnings);
    let window = o.text("refresh_window").and_then(|w| w.map(|w| parse_window(&w)).transpose());
    let refresh_window = or_warn(window, None, warnings);
    let channel = or_warn(o.channel(), None, warnings);
    Global { refresh_interval, enable_auto_refresh, refresh_window, channel }
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
            virtualenv: o.text("virtualenv")?.or(o.text("env")?).map(|p| expand_home(&p)),
            requirements: o.text("requirements")?,
            system_dependencies: o.text("system_dependencies")?,
            install_script: o.text("install_script")?,
        }),
        Some(t @ ("web" | "zip" | "python" | "executable")) => {
            return Err(format!("[{}]: orrery doesn't update {t} entries yet, so this entry is left out", o.section));
        }
        Some(t) => return Err(format!("[{}] type: orrery doesn't know the type {t:?}", o.section)),
        None => return Err(format!("[{}] needs a type option, such as git_repo", o.section)),
    };
    let channel = o.channel()?.or(global.channel).unwrap_or(Channel::Dev);
    Ok(Entry {
        name: name.to_owned(),
        channel,
        refresh_interval: o.hours("refresh_interval")?.unwrap_or(global.refresh_interval),
        managed_services: o.map.contains_key("managed_services").then(|| o.list("managed_services")).transpose()?,
        is_system_service: o.bool("is_system_service", true)?,
        info_tags: o.list("info_tags")?,
        report_anomalies: o.bool("report_anomalies", true)?,
        hash: hash(map),
        kind,
    })
}

impl Entry {
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
        let Kind::Git(git) = &e.kind;
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
        let Kind::Git(git) = &e.kind;
        assert_eq!(git.primary_branch, "master");

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
        assert!(err("Klipper", json!({"type": "git_repo"})).contains("Moonraker uses"));
        assert!(
            err("x", json!({"type": "git_repo", "path": "/x", "origin": "o", "is_system_service": "maybe"}))
                .contains("expected true or false")
        );
    }
}
