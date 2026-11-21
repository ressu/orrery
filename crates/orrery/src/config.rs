//! orrery's config: `moonraker.conf`, read for convenience, and orrery's own `orrery.toml`.
//!
//! `moonraker.conf` follows Moonraker's rules (`confighelper.py`): `key: value` or
//! `key = value`, full-line and inline (` #`, ` ;`) comments, indented continuation lines, and
//! `[include <glob>]`.
//!
//! `orrery.toml` uses the same section and option names, as TOML tables. A table inside a table
//! is a named section: `[webcam.front]` is `[webcam front]` in `moonraker.conf`, and
//! `[agent.spoolman]` is the agent `spoolman`. Inside a named section, a further table is one
//! option with a table as its value, such as an agent's `maps`. When both files set an option,
//! `orrery.toml` wins.
//!
//! Components read options through [`Section`] getters with defaults. Every read is recorded, so
//! `server.config` can report the parsed values the way Moonraker does. `server.config`'s `orig`
//! stays Moonraker's: the text of `moonraker.conf`, so clients written for Moonraker find what
//! they expect.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

#[derive(Debug, Default)]
pub struct Config {
    /// Sections in file order, with options in file order. Option names are lowercase.
    sections: Vec<(String, Vec<(String, String)>)>,
    /// `orrery.toml`'s sections, with typed values.
    typed: Vec<(String, Vec<(String, Value)>)>,
    /// `orrery.toml`, when it was read.
    toml_file: Option<PathBuf>,
    /// Each file read, with the sections it defines.
    files: Vec<(PathBuf, Vec<String>)>,
    /// Values read by components, as `server.config` reports them under `config`.
    parsed: Mutex<Map<String, Value>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = Config::default();
        let mut visited = BTreeSet::new();
        config.parse_file(path, &mut visited)?;
        Ok(config)
    }

    /// Reads `orrery.toml` over the `moonraker.conf` already loaded.
    pub fn load_toml(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
        self.parse_toml(&text, path)
    }

    pub(crate) fn parse_toml(&mut self, text: &str, path: &Path) -> Result<()> {
        let table: toml::Table =
            toml::from_str(text).with_context(|| format!("{} isn't valid TOML", path.display()))?;
        let mut names = Vec::new();
        for (name, value) in table {
            let toml::Value::Table(options) = value else {
                bail!("{}: `{name}` must be inside a table, such as [server]", path.display());
            };
            let mut plain = Vec::new();
            for (key, value) in options {
                match value {
                    toml::Value::Table(named) => {
                        let section = format!("{name} {key}");
                        let named = named.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), json_value(v))).collect();
                        self.typed.push((section.clone(), named));
                        names.push(section);
                    }
                    other => plain.push((key.to_ascii_lowercase(), json_value(other))),
                }
            }
            if !plain.is_empty() || !names.iter().any(|n| n.starts_with(&format!("{name} "))) {
                self.typed.push((name.clone(), plain));
                names.push(name);
            }
        }
        self.files.push((path.to_path_buf(), names));
        self.toml_file = Some(path.to_path_buf());
        Ok(())
    }

    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self> {
        let mut config = Config::default();
        config.parse_text(text, Path::new("moonraker.conf"), &mut BTreeSet::new())?;
        Ok(config)
    }

    fn parse_file(&mut self, path: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let canonical = path.canonicalize().with_context(|| format!("can't open {}", path.display()))?;
        if !visited.insert(canonical.clone()) {
            bail!("{} includes itself", path.display());
        }
        let text = std::fs::read_to_string(&canonical).with_context(|| format!("can't read {}", path.display()))?;
        self.parse_text(&text, path, visited)
    }

    fn parse_text(&mut self, text: &str, path: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let file_index = self.files.len();
        self.files.push((path.to_path_buf(), Vec::new()));
        let mut section: Option<usize> = None;
        let mut option_indent: Option<usize> = None;
        for (lineno, raw) in text.lines().enumerate() {
            let trimmed = raw.trim_start();
            if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
                continue;
            }
            let mut line = raw.replace('\t', "    ");
            if let Some(pos) = inline_comment(&line) {
                line.truncate(pos);
            }
            let line = line.replace(" \\#", " #").replace(" \\;", " ;");
            let indent = line.len() - line.trim_start().len();
            if let Some(oi) = option_indent
                && indent > oi
            {
                let (_, options) = &mut self.sections[section.expect("an option implies a section")];
                let value = &mut options.last_mut().expect("an indent implies an option").1;
                value.push('\n');
                value.push_str(line.trim());
                continue;
            }
            let content = line.trim();
            if let Some(name) = content.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                option_indent = None;
                let name = name.trim();
                if let Some(pattern) = name.strip_prefix("include ") {
                    self.include(pattern.trim(), path, visited)?;
                    section = None;
                    continue;
                }
                section = Some(match self.sections.iter().position(|(n, _)| n == name) {
                    Some(i) => i,
                    None => {
                        self.sections.push((name.to_owned(), Vec::new()));
                        self.sections.len() - 1
                    }
                });
                let file_sections = &mut self.files[file_index].1;
                if !file_sections.iter().any(|s| s == name) {
                    file_sections.push(name.to_owned());
                }
                continue;
            }
            let Some(sidx) = section else {
                bail!("{}:{}: option outside a section: {content}", path.display(), lineno + 1);
            };
            let Some(sep) = content.find([':', '=']) else {
                bail!("{}:{}: expected `option: value`, got {content:?}", path.display(), lineno + 1);
            };
            let key = content[..sep].trim().to_ascii_lowercase();
            let value = content[sep + 1..].trim().to_owned();
            let options = &mut self.sections[sidx].1;
            match options.iter_mut().find(|(k, _)| *k == key) {
                Some(existing) => existing.1 = value,
                None => options.push((key, value)),
            }
            option_indent = Some(indent);
        }
        Ok(())
    }

    fn include(&mut self, pattern: &str, from: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let base =
            if pattern.starts_with('/') { PathBuf::new() } else { from.parent().unwrap_or(Path::new(".")).into() };
        let full = base.join(pattern);
        let dir = full.parent().unwrap_or(Path::new("."));
        let name_pattern = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("[include {pattern}]: can't read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| glob_match(&name_pattern, &n.to_string_lossy())))
            .collect();
        if paths.is_empty() {
            bail!("no files match [include {pattern}]");
        }
        paths.sort();
        for p in paths {
            self.parse_file(&p, visited)?;
        }
        Ok(())
    }

    pub fn section(&self, name: &str) -> Section<'_> {
        let options = self.sections.iter().find(|(n, _)| n == name).map(|(_, o)| o.as_slice()).unwrap_or(&[]);
        let typed = self.typed.iter().find(|(n, _)| n == name).map(|(_, o)| o.as_slice()).unwrap_or(&[]);
        let mut parsed = self.parsed.lock().expect("config lock");
        parsed.entry(name.to_owned()).or_insert_with(|| Value::Object(Map::new()));
        Section { config: self, name: name.to_owned(), options, typed }
    }

    /// A section's options as JSON, without recording them as read: `orrery.toml`'s values with
    /// their types, over `moonraker.conf`'s text. `None` if neither file has the section.
    pub fn section_json(&self, name: &str) -> Option<Map<String, Value>> {
        let ini = self.sections.iter().find(|(n, _)| n == name);
        let typed = self.typed.iter().find(|(n, _)| n == name);
        if ini.is_none() && typed.is_none() {
            return None;
        }
        let mut out = Map::new();
        for (k, v) in ini.map(|(_, o)| o.as_slice()).unwrap_or_default() {
            out.insert(k.clone(), json!(v));
        }
        for (k, v) in typed.map(|(_, o)| o.as_slice()).unwrap_or_default() {
            out.insert(k.clone(), v.clone());
        }
        Some(out)
    }

    /// The names of sections that start with `prefix`, such as every `[webcam <name>]`, from
    /// both files.
    pub fn section_names(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> =
            self.sections.iter().map(|(n, _)| n).filter(|n| n.starts_with(prefix)).cloned().collect();
        for (n, _) in &self.typed {
            if n.starts_with(prefix) && !names.contains(n) {
                names.push(n.clone());
            }
        }
        names
    }

    /// `server.config`'s `orig`: every section's raw options, after a `DEFAULT` entry.
    pub fn orig(&self) -> Value {
        let mut out = Map::new();
        out.insert("DEFAULT".into(), json!({}));
        for (name, options) in &self.sections {
            let opts: Map<String, Value> = options.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            out.insert(name.clone(), Value::Object(opts));
        }
        Value::Object(out)
    }

    /// Reads the options Moonraker's components read and orrery doesn't, with Moonraker's
    /// defaults, so `server.config` lists them as Moonraker does: Fluidd and Mainsail show that
    /// list. It also lists every component's section in Moonraker's load order, so call it
    /// before orrery's components read their options.
    pub fn record_moonraker_options(&self) -> Result<()> {
        use Moonraker::*;
        for (name, options) in MOONRAKER_OPTIONS {
            let s = self.section(name);
            for (key, default) in *options {
                match default {
                    Int(d) => drop(s.get_int(key, *d)?),
                    OptInt => match s.get_value(key) {
                        Some(_) => drop(s.get_int(key, 0)?),
                        None => s.record(key, Value::Null),
                    },
                    Float(d) => drop(s.get_float(key, *d)?),
                    Bool(d) => drop(s.get_bool(key, *d)?),
                    Str(d) => drop(s.get_str(key, d)),
                    OptStr => drop(s.get_opt_str(key)),
                    List => drop(s.get_list(key)),
                }
            }
        }
        Ok(())
    }

    pub fn parsed(&self) -> Value {
        Value::Object(self.parsed.lock().expect("config lock").clone())
    }

    /// `server.config`'s `files`: each file's name relative to the main file's folder.
    pub fn files(&self) -> Value {
        let base = self.files.first().and_then(|(p, _)| p.parent()).map(Path::to_path_buf).unwrap_or_default();
        let list: Vec<Value> = self
            .files
            .iter()
            .map(|(path, sections)| {
                let name = path.strip_prefix(&base).unwrap_or(path).display().to_string();
                json!({"filename": name, "sections": sections})
            })
            .collect();
        Value::Array(list)
    }
}

/// A TOML value as JSON. Dates and times become their TOML text.
fn json_value(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Datetime(d) => json!(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(json_value).collect()),
        toml::Value::Table(t) => Value::Object(t.into_iter().map(|(k, v)| (k, json_value(v))).collect()),
    }
}

/// Finds an inline comment: a `#` or `;` preceded by at least one space.
fn inline_comment(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    (1..bytes.len()).find(|&i| matches!(bytes[i], b'#' | b';') && bytes[i - 1] == b' ').map(|i| {
        let mut start = i;
        while start > 0 && bytes[start - 1] == b' ' {
            start -= 1;
        }
        start
    })
}

/// Shell-style `*` and `?` matching, enough for `[include]` file names.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// One section's options. Each getter records the value it returns in `server.config`.
pub struct Section<'c> {
    config: &'c Config,
    name: String,
    options: &'c [(String, String)],
    /// The same section in `orrery.toml`, which wins over `moonraker.conf`.
    typed: &'c [(String, Value)],
}

impl Section<'_> {
    fn raw(&self, key: &str) -> Option<&str> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn typed(&self, key: &str) -> Option<&Value> {
        self.typed.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn typed_error(&self, key: &str, what: &str, got: &Value) -> anyhow::Error {
        let file = self.config.toml_file.as_deref().unwrap_or(Path::new("orrery.toml"));
        anyhow::anyhow!("{}: [{}] {key}: {what}, got {got}", file.display(), self.name)
    }

    /// Whether the option is set, in either file. Unlike the getters, it doesn't list the
    /// option in `server.config`: for orrery's own options, which Moonraker doesn't have.
    pub fn has(&self, key: &str) -> bool {
        self.typed(key).is_some() || self.raw(key).is_some()
    }

    /// An option's value as JSON: from `orrery.toml` as it's typed there, or the text from
    /// `moonraker.conf`. For options that hold a table or a list, such as an agent's `maps`.
    pub fn get_value(&self, key: &str) -> Option<Value> {
        let v = self.typed(key).cloned().or_else(|| self.raw(key).map(|s| json!(s)));
        self.record(key, v.clone().unwrap_or_default());
        v
    }

    fn record(&self, key: &str, value: Value) {
        let mut parsed = self.config.parsed.lock().expect("config lock");
        if let Some(Value::Object(section)) = parsed.get_mut(&self.name) {
            section.insert(key.to_owned(), value);
        }
    }

    fn error(&self, key: &str, what: &str) -> anyhow::Error {
        anyhow::anyhow!("[{}] {key}: {what}, got {:?}", self.name, self.raw(key).unwrap_or_default())
    }

    /// A string. From `orrery.toml`, a number or a boolean is taken as its text.
    pub fn get_str(&self, key: &str, default: &str) -> String {
        let v = self.get_opt_str_unrecorded(key).unwrap_or_else(|| default.to_owned());
        self.record(key, json!(v));
        v
    }

    pub fn get_opt_str(&self, key: &str) -> Option<String> {
        let v = self.get_opt_str_unrecorded(key);
        self.record(key, json!(v));
        v
    }

    fn get_opt_str_unrecorded(&self, key: &str) -> Option<String> {
        match self.typed(key) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(other) => Some(other.to_string()),
            None => self.raw(key).map(str::to_owned),
        }
    }

    /// A list: a TOML array, or one item per line, as Moonraker's `getlist`. Blank lines are
    /// skipped.
    pub fn get_list(&self, key: &str) -> Vec<String> {
        let v: Vec<String> = match self.typed(key) {
            Some(Value::Array(items)) => {
                items.iter().map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned)).collect()
            }
            Some(Value::String(s)) => lines(s),
            Some(other) => vec![other.to_string()],
            None => lines(self.raw(key).unwrap_or_default()),
        };
        self.record(key, json!(v));
        v
    }

    pub fn get_int(&self, key: &str, default: i64) -> Result<i64> {
        let v = match (self.typed(key), self.raw(key)) {
            (Some(t), _) => t.as_i64().ok_or_else(|| self.typed_error(key, "expected a whole number", t))?,
            (None, Some(s)) => s.parse().map_err(|_| self.error(key, "expected a whole number"))?,
            (None, None) => default,
        };
        self.record(key, json!(v));
        Ok(v)
    }

    pub fn get_float(&self, key: &str, default: f64) -> Result<f64> {
        let v = match (self.typed(key), self.raw(key)) {
            (Some(t), _) => t.as_f64().ok_or_else(|| self.typed_error(key, "expected a number", t))?,
            (None, Some(s)) => s.parse().map_err(|_| self.error(key, "expected a number"))?,
            (None, None) => default,
        };
        self.record(key, json!(v));
        Ok(v)
    }

    pub fn get_bool(&self, key: &str, default: bool) -> Result<bool> {
        let v = match (self.typed(key), self.raw(key).map(str::to_ascii_lowercase).as_deref()) {
            (Some(t), _) => t.as_bool().ok_or_else(|| self.typed_error(key, "expected true or false", t))?,
            (None, Some("true" | "yes" | "on" | "1")) => true,
            (None, Some("false" | "no" | "off" | "0")) => false,
            (None, Some(_)) => return Err(self.error(key, "expected true or false")),
            (None, None) => default,
        };
        self.record(key, json!(v));
        Ok(v)
    }
}

fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

/// The default of an option in [`MOONRAKER_OPTIONS`].
enum Moonraker {
    Int(i64),
    /// A whole number, or `null` when it isn't set.
    OptInt,
    Float(f64),
    Bool(bool),
    Str(&'static str),
    /// A string, or `null` when it isn't set.
    OptStr,
    List,
}

/// Moonraker's components, in the order it loads them, and the options each reads that orrery
/// doesn't. Options orrery reads itself are recorded when it reads them.
const MOONRAKER_OPTIONS: &[(&str, &[(&str, Moonraker)])] = &[
    (
        "server",
        &[
            ("ssl_port", Moonraker::Int(7130)),
            ("enable_debug_logging", Moonraker::Bool(false)),
            ("use_xheaders", Moonraker::Bool(true)),
            ("ssl_certificate_path", Moonraker::OptStr),
            ("ssl_key_path", Moonraker::OptStr),
            ("route_prefix", Moonraker::OptStr),
            ("max_websocket_connections", Moonraker::Int(50)),
        ],
    ),
    ("template", &[]),
    ("secrets", &[("secrets_path", Moonraker::OptStr)]),
    ("database", &[]),
    ("dbus_manager", &[]),
    (
        "file_manager",
        &[
            ("enable_object_processing", Moonraker::Bool(false)),
            ("default_metadata_parser_timeout", Moonraker::Float(20.0)),
            ("enable_observer_warnings", Moonraker::Bool(true)),
            ("enable_inotify_warnings", Moonraker::Bool(true)),
            ("config_path", Moonraker::OptStr),
            ("log_path", Moonraker::OptStr),
        ],
    ),
    (
        "authorization",
        &[
            ("login_timeout", Moonraker::Int(90)),
            ("force_logins", Moonraker::Bool(false)),
            ("default_source", Moonraker::Str("moonraker")),
            ("enable_api_key", Moonraker::Bool(true)),
            ("max_login_attempts", Moonraker::OptInt),
        ],
    ),
    ("klippy_apis", &[]),
    (
        "machine",
        &[
            ("sudo_password", Moonraker::OptStr),
            ("shutdown_action", Moonraker::Str("poweroff")),
            ("force_validation", Moonraker::Bool(false)),
            ("validate_service", Moonraker::Bool(true)),
            ("validate_config", Moonraker::Bool(true)),
        ],
    ),
    ("shell_command", &[]),
    ("data_store", &[]),
    ("proc_stats", &[]),
    ("job_state", &[]),
    ("job_queue", &[]),
    ("history", &[]),
    ("http_client", &[]),
    (
        "announcements",
        &[
            ("enable_moonlight", Moonraker::Bool(true)),
            ("dev_mode", Moonraker::Bool(false)),
            ("subscriptions", Moonraker::List),
        ],
    ),
    ("webcam", &[]),
    ("extensions", &[]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_comments_and_multiline() {
        let c = Config::parse(
            "# comment\n[server]\nhost: 0.0.0.0  # inline\nPort = 7125\n\n[authorization]\ntrusted_clients:\n  10.0.0.0/8\n  127.0.0.1\n",
        )
        .unwrap();
        let s = c.section("server");
        assert_eq!(s.get_str("host", ""), "0.0.0.0");
        assert_eq!(s.get_int("port", 0).unwrap(), 7125);
        assert_eq!(c.orig()["authorization"]["trusted_clients"], json!("\n10.0.0.0/8\n127.0.0.1"));
        assert_eq!(c.parsed()["server"]["port"], json!(7125));
        assert_eq!(c.orig()["server"]["host"], json!("0.0.0.0"));
    }

    #[test]
    fn missing_section_gives_defaults() {
        let c = Config::parse("[server]\n").unwrap();
        assert!(!c.section("history").get_bool("x", false).unwrap());
        assert_eq!(c.parsed()["history"]["x"], json!(false));
    }

    #[test]
    fn toml_wins_and_keeps_types() {
        let mut c =
            Config::parse("[server]\nhost: 0.0.0.0\nport: 7125\n[webcam front]\nservice: mjpegstreamer\n").unwrap();
        c.parse_toml(
            r#"
[server]
port = 7126

[file_manager]
queue_gcode_uploads = true

[webcam.front]
service = "webrtc-camerastreamer"

[webcam.back]
location = "door"

[agent.spoolman]
command = "~/orrery/agents/orrery-spoolman"
history_fields = ["spool_ids"]

[agent.spoolman.maps]
"server.spoolman" = "agent.spoolman"
"#,
            Path::new("orrery.toml"),
        )
        .unwrap();
        let server = c.section("server");
        assert_eq!(server.get_int("port", 0).unwrap(), 7126);
        assert_eq!(server.get_str("host", ""), "0.0.0.0");
        drop(server);
        assert!(c.section("file_manager").get_bool("queue_gcode_uploads", false).unwrap());
        assert_eq!(c.section("webcam front").get_str("service", ""), "webrtc-camerastreamer");
        assert_eq!(c.section_names("webcam "), vec!["webcam front", "webcam back"]);
        let agent = c.section("agent spoolman");
        assert_eq!(agent.get_list("history_fields"), vec!["spool_ids"]);
        assert_eq!(agent.get_value("maps"), Some(json!({"server.spoolman": "agent.spoolman"})));
        drop(agent);
        // `orig` stays moonraker.conf's text; the parsed values are the ones in effect.
        assert_eq!(c.orig()["server"]["port"], json!("7125"));
        assert_eq!(c.parsed()["server"]["port"], json!(7126));
        assert_eq!(
            c.files()[1],
            json!({"filename": "orrery.toml", "sections":
            ["server", "file_manager", "webcam front", "webcam back", "agent spoolman"]})
        );
    }

    #[test]
    fn toml_type_errors_name_the_file() {
        let mut c = Config::parse("[server]\n").unwrap();
        c.parse_toml("[server]\nport = \"7126\"\n", Path::new("/cfg/orrery.toml")).unwrap();
        let err = c.section("server").get_int("port", 0).unwrap_err().to_string();
        assert_eq!(err, "/cfg/orrery.toml: [server] port: expected a whole number, got \"7126\"");
        let mut c = Config::parse("[server]\n").unwrap();
        assert!(c.parse_toml("port = 1\n", Path::new("orrery.toml")).is_err());
        assert!(c.parse_toml("[server\n", Path::new("orrery.toml")).is_err());
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.conf", "a.conf"));
        assert!(!glob_match("*.conf", "a.cfg"));
        assert!(glob_match("x?.conf", "x1.conf"));
    }
}
