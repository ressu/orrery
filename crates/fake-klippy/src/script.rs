//! What a fake Klippy serves: a capture recorded from real Klipper, plus overrides from a script.

use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// State recorded from a real Klipper with `fake-klippy capture`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capture {
    /// Response to `info`. `state`, `state_message` and `process_id` are replaced at runtime.
    pub info: Map<String, Value>,
    /// Response to `list_endpoints`.
    pub endpoints: Vec<String>,
    /// Full `get_status()` of every printer object, as `objects/query` returns it.
    pub objects: Map<String, Value>,
    /// Response to `gcode/help`.
    pub gcode_help: Map<String, Value>,
}

/// Klippy's printer state, as reported by `info` and the `webhooks` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KlippyState {
    Startup,
    Ready,
    Error,
    Shutdown,
}

impl KlippyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Shutdown => "shutdown",
        }
    }

    /// The `state_message` real Klipper reports for this state, where it has a fixed one.
    pub fn default_message(self) -> &'static str {
        match self {
            Self::Startup => "Printer is not ready",
            Self::Ready => "Printer is ready",
            Self::Error => "Printer error",
            Self::Shutdown => "Shutdown due to webhooks request",
        }
    }
}

/// How the fake answers one gcode line. Rules are tried in order; the first match wins.
#[derive(Debug, Clone, Deserialize)]
pub struct GcodeRule {
    /// Regex matched against the whole line (trimmed). Named groups can be used as `{name}` in
    /// `respond` and `set` strings.
    #[serde(rename = "match", with = "serde_regex")]
    pub pattern: Regex,
    /// Lines sent to `gcode/subscribe_output` subscribers.
    #[serde(default)]
    pub respond: Vec<String>,
    /// Fail the `gcode/script` request with this message.
    #[serde(default)]
    pub error: Option<String>,
    /// Printer object fields to change, as `{object: {field: value}}`.
    #[serde(default)]
    pub set: Map<String, Value>,
}

/// A script file: a capture to start from, plus changes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptFile {
    /// Path to a capture JSON file, relative to the script file.
    #[serde(default)]
    pub capture: Option<String>,
    /// State when the fake starts. Defaults to `ready`.
    #[serde(default)]
    pub state: Option<KlippyState>,
    #[serde(default)]
    pub state_message: Option<String>,
    /// Stay in `startup` this long before switching to `state`.
    #[serde(default)]
    pub startup_ms: u64,
    /// How long a RESTART or FIRMWARE_RESTART keeps the fake in `startup`.
    #[serde(default = "default_restart_ms")]
    pub restart_ms: u64,
    /// Object fields merged over the capture, as `{object: {field: value}}`.
    #[serde(default)]
    pub objects: Map<String, Value>,
    /// Extra endpoints to list.
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub gcode: Vec<GcodeRule>,
}

fn default_restart_ms() -> u64 {
    500
}

/// Everything a [`crate::FakeKlippy`] needs to start.
#[derive(Debug, Clone)]
pub struct Script {
    pub capture: Capture,
    pub state: KlippyState,
    pub state_message: Option<String>,
    pub startup_ms: u64,
    pub restart_ms: u64,
    pub gcode: Vec<GcodeRule>,
}

impl Script {
    /// Loads a YAML script file and the capture it names.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: ScriptFile = serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::from_script_file(file, base)
    }

    /// Builds a script from an already-parsed file; `base` resolves the capture path.
    pub fn from_script_file(file: ScriptFile, base: &Path) -> Result<Self> {
        let mut capture = match &file.capture {
            Some(rel) => {
                let path = base.join(rel);
                let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            None => Capture::default(),
        };
        merge_objects(&mut capture.objects, &file.objects);
        for ep in file.endpoints {
            if !capture.endpoints.contains(&ep) {
                capture.endpoints.push(ep);
            }
        }
        Ok(Self {
            capture,
            state: file.state.unwrap_or(KlippyState::Ready),
            state_message: file.state_message,
            startup_ms: file.startup_ms,
            restart_ms: file.restart_ms,
            gcode: file.gcode,
        })
    }

    /// Points `virtual_sdcard` at `path`, in both the live object and the parsed config, the way
    /// a real printer.cfg would. Moonraker reads the path from `configfile` at startup.
    pub fn set_sdcard_path(&mut self, path: &str) {
        set_sdcard_path(&mut self.capture.objects, path);
    }
}

/// Sets `[virtual_sdcard] path` in the `configfile` object's `config` and `settings`.
pub fn set_sdcard_path(objects: &mut Map<String, Value>, path: &str) {
    if let Some(Value::Object(cf)) = objects.get_mut("configfile") {
        for key in ["config", "settings"] {
            if let Some(Value::Object(vsd)) = cf.get_mut(key).and_then(|c| c.get_mut("virtual_sdcard")) {
                vsd.insert("path".into(), Value::String(path.into()));
            }
        }
    }
}

/// Merges `{object: {field: value}}` into `objects`, replacing whole field values. This matches
/// how Klipper reports changes: per field, never deeper.
pub fn merge_objects(objects: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (name, fields) in patch {
        let entry = objects.entry(name.clone()).or_insert_with(|| Value::Object(Map::new()));
        match (entry, fields) {
            (Value::Object(dst), Value::Object(src)) => {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            (dst, src) => *dst = src.clone(),
        }
    }
}

mod serde_regex {
    use regex::Regex;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Regex, D::Error> {
        let s = String::deserialize(d)?;
        Regex::new(&s).map_err(serde::de::Error::custom)
    }
}
