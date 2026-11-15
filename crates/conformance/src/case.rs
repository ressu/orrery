//! Test case files (`conformance/cases/<area>/<name>.yaml`).
//!
//! ```yaml
//! name: database round trip
//! klippy: ready            # fake-klippy script name, `none` for no Klippy at all
//! transports: [ws, http]   # run the whole case once per transport
//! steps:
//!   - call: server.database.post_item
//!     params: {namespace: test, key: a, value: 1}
//!     expect: {namespace: test, key: a, value: 1}
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Transport used for `call` steps. Notifications always arrive over the websocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Ws,
    Http,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::Http => "http",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// fake-klippy script from `conformance/klippy-scripts/<name>.yaml`, or `none`.
    #[serde(default = "default_klippy")]
    pub klippy: String,
    /// Wait for Klippy to be ready before the first step. Defaults to true when `klippy` is set.
    #[serde(default)]
    pub wait_ready: Option<bool>,
    #[serde(default = "default_transports")]
    pub transports: Vec<Transport>,
    /// Extra `moonraker.conf` text appended to the base config.
    #[serde(default)]
    pub config: Option<String>,
    /// Tags select tiers: `real-klippy` cases also run against real Klipper.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Known difference from Moonraker (an id in docs/regressions.md). Failures are reported but
    /// don't fail the run; a pass is reported so the entry can be removed.
    #[serde(default)]
    pub known_regression: Option<String>,
    /// Response paths whose values vary between runs, ignored in snapshot comparisons.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Files written into the data directory before the server starts: `{relative path: text}`,
    /// for example `gcodes/cube.gcode: "G28\n"`.
    #[serde(default)]
    pub files: Map<String, Value>,
    pub steps: Vec<Step>,

    /// Set by the loader: `<area>/<file stem>`.
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub path: PathBuf,
}

fn default_klippy() -> String {
    "ready".into()
}

fn default_transports() -> Vec<Transport> {
    vec![Transport::Ws, Transport::Http]
}

impl Case {
    pub fn uses_klippy(&self) -> bool {
        self.klippy != "none"
    }

    pub fn waits_ready(&self) -> bool {
        self.wait_ready.unwrap_or(self.uses_klippy())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Step {
    Call(Box<CallStep>),
    Notification(NotificationStep),
    NoNotification(NoNotificationStep),
    Klippy(KlippyStep),
    KlippyRequest(KlippyRequestStep),
    Raw(RawStep),
    Upload(Box<UploadStep>),
    Download(DownloadStep),
    Sleep(SleepStep),
}

/// Calls a method. With neither `expect` nor `expect_error`, any successful reply passes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallStep {
    pub call: String,
    #[serde(default)]
    pub params: Option<Value>,
    /// Matcher for the `result`.
    #[serde(default)]
    pub expect: Option<Value>,
    /// Matcher for the error object (`{code, message}`). The call must fail.
    #[serde(default)]
    pub expect_error: Option<Value>,
    /// Retry until the expectation holds or this many milliseconds pass.
    #[serde(default)]
    pub retry_ms: Option<u64>,
    /// Store values from the result for `${name}` substitution in later steps: `{name: path}`,
    /// where `path` is dotted (`job.job_id`, `items.0`).
    #[serde(default)]
    pub save: Map<String, Value>,
    /// Restrict this step to some transports; it's skipped on the others.
    #[serde(default)]
    pub only: Option<Vec<Transport>>,
    /// Use this HTTP request instead of the route table (needed for GET requests whose params
    /// aren't plain scalars).
    #[serde(default)]
    pub http: Option<HttpOverride>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOverride {
    pub verb: String,
    pub path: String,
    /// Query string, already encoded (`a=1&b`).
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
}

/// Waits for a websocket notification whose `params` match.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationStep {
    pub expect_notification: String,
    #[serde(default, rename = "match")]
    pub matches: Option<Value>,
    #[serde(default = "default_within")]
    pub within_ms: u64,
}

/// Fails if a notification arrives within the window.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoNotificationStep {
    pub expect_no_notification: String,
    #[serde(default, rename = "match")]
    pub matches: Option<Value>,
    #[serde(default = "default_quiet")]
    pub within_ms: u64,
}

fn default_within() -> u64 {
    3000
}

fn default_quiet() -> u64 {
    500
}

/// Drives the fake Klippy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KlippyStep {
    pub klippy: KlippyAction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KlippyAction {
    /// Change printer object fields: `{object: {field: value}}`.
    Update(Map<String, Value>),
    /// Emit a line of gcode output.
    GcodeOutput(String),
    /// Switch state: `{state: shutdown, message: "..."}`.
    State {
        state: fake_klippy::KlippyState,
        #[serde(default)]
        message: Option<String>,
    },
    /// RESTART: drop clients, then `startup` until the script's `restart_ms` passes.
    Restart,
    /// Drop every client connection.
    Disconnect,
    /// Call a method registered with `register_remote_method`.
    RemoteMethod {
        method: String,
        #[serde(default)]
        params: Option<Value>,
    },
}

/// Asserts that the fake Klippy received a request matching `{method, params}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KlippyRequestStep {
    pub expect_klippy_request: Value,
    #[serde(default = "default_within")]
    pub within_ms: u64,
}

/// Sends a raw websocket text frame and matches the raw reply.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStep {
    pub raw_ws: String,
    #[serde(default)]
    pub expect: Option<Value>,
}

/// Uploads a file with a multipart `POST /server/files/upload`, as frontends do. It goes over
/// HTTP on both transports.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadStep {
    /// The file name sent with the file, which can include folders: `dir/cube.gcode`.
    pub upload: String,
    /// The file's text.
    #[serde(default)]
    pub content: Option<String>,
    /// A file under `conformance/fixtures/files/` to send instead of `content`.
    #[serde(default)]
    pub fixture: Option<String>,
    /// Other form fields, such as `root`, `path`, `print`, or `checksum`.
    #[serde(default)]
    pub form: Map<String, Value>,
    #[serde(default)]
    pub expect: Option<Value>,
    #[serde(default)]
    pub expect_error: Option<Value>,
}

/// Downloads `GET /server/files/<root>/<path>`. It goes over HTTP on both transports.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadStep {
    /// `<root>/<path>`, for example `gcodes/dir/cube.gcode`.
    pub download: String,
    /// The HTTP status to expect. Defaults to 200.
    #[serde(default = "default_status")]
    pub expect_status: u16,
    /// The exact body to expect.
    #[serde(default)]
    pub expect_body: Option<String>,
    /// The body must be identical to this file under `conformance/fixtures/files/`.
    #[serde(default)]
    pub expect_fixture: Option<String>,
    /// The `Content-Type` header to expect.
    #[serde(default)]
    pub expect_content_type: Option<String>,
}

fn default_status() -> u16 {
    200
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SleepStep {
    pub sleep_ms: u64,
}

/// Loads every `*.yaml` under `dir`, sorted by id.
pub fn load_all(dir: &Path) -> Result<Vec<Case>> {
    let mut cases = Vec::new();
    let mut areas: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    areas.sort_by_key(|e| e.file_name());
    for area in areas {
        let mut files: Vec<_> = std::fs::read_dir(area.path())?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
            .collect();
        files.sort();
        for path in files {
            cases.push(load(&path, &area.file_name().to_string_lossy())?);
        }
    }
    Ok(cases)
}

fn load(path: &Path, area: &str) -> Result<Case> {
    let text = std::fs::read_to_string(path)?;
    let mut case: Case = serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    case.id = format!("{area}/{}", path.file_stem().unwrap_or_default().to_string_lossy());
    case.path = path.to_path_buf();
    if case.steps.is_empty() {
        bail!("{}: no steps", path.display());
    }
    Ok(case)
}
