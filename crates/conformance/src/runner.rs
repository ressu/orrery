//! Runs one case on one transport against a fresh server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use fake_klippy::{FakeKlippy, Script};
use serde_json::{Map, Value, json};

use crate::case::{Case, KlippyAction, Step, Transport};
use crate::client::{HttpClient, Outcome, Route, WsClient};
use crate::matcher::{mismatches, short};
use crate::target::{DataDir, Paths, RealKlippy, Server, TargetKind, free_port};

const READY_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Settings {
    pub target: TargetKind,
    pub paths: Paths,
    pub routes: HashMap<String, Route>,
    /// Keep data directories of failed runs for debugging.
    pub keep_failed: bool,
    /// Use real Klipper instead of the fake. Only `real-klippy` cases are run this way.
    pub real_klipper: bool,
}

/// The outcome of one case on one transport.
pub struct RunResult {
    pub case_id: String,
    pub transport: Transport,
    pub failures: Vec<String>,
    /// Call outcomes in step order, for snapshots: `{"step", "call", "result" | "error"}`.
    pub transcript: Vec<Value>,
    /// Data directory left behind for a failed run, when `keep_failed` is set.
    pub kept: Option<String>,
}

impl RunResult {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

pub async fn run_case(settings: &Settings, case: &Case, transport: Transport) -> RunResult {
    let mut result =
        RunResult { case_id: case.id.clone(), transport, failures: Vec::new(), transcript: Vec::new(), kept: None };
    let data = match DataDir::create(&settings.paths, &format!("{}-{}", case.id, transport.as_str())) {
        Ok(d) => d,
        Err(e) => {
            result.failures.push(format!("setup: {e:#}"));
            return result;
        }
    };
    if let Err(e) = run_steps(settings, case, transport, &data, &mut result).await {
        result.failures.push(format!("{e:#}"));
    }
    if !result.passed() && settings.keep_failed {
        let path = data.dir.keep();
        result.kept = Some(path.display().to_string());
    }
    result
}

struct Ctx {
    transport: Transport,
    ws: WsClient,
    http: HttpClient,
    fake: Option<FakeKlippy>,
    real: Option<RealKlippy>,
    vars: Map<String, Value>,
    /// `conformance/fixtures/files`, for upload and download steps.
    fixtures: PathBuf,
}

async fn run_steps(
    settings: &Settings,
    case: &Case,
    transport: Transport,
    data: &DataDir,
    result: &mut RunResult,
) -> Result<()> {
    let port = free_port()?;
    data.write_config(&settings.paths, port, case.config.as_deref())?;
    data.write_files(&case.files)?;
    let real = if case.uses_klippy() && settings.real_klipper {
        Some(RealKlippy::start(&settings.paths, data).await?)
    } else {
        None
    };
    let fake = if case.uses_klippy() && !settings.real_klipper {
        let script_path = settings.paths.conformance().join(format!("klippy-scripts/{}.yaml", case.klippy));
        let mut script = Script::from_file(&script_path)?;
        script.set_sdcard_path(&data.path().join("gcodes").to_string_lossy());
        Some(FakeKlippy::start(data.klippy_socket(), script).await.context("starting fake klippy")?)
    } else {
        None
    };
    let server = Server::start(settings.target, &settings.paths, data, port).await?;
    let ws = WsClient::connect(&server.ws_url()).await?;
    let http = HttpClient::new(server.base_url(), settings.routes.clone());
    let fixtures = settings.paths.conformance().join("fixtures/files");
    let mut ctx = Ctx { transport, ws, http, fake, real, vars: Map::new(), fixtures };

    if case.waits_ready() {
        wait_klippy_ready(&ctx).await?;
        ctx.ws.drain();
    }
    for (i, step) in case.steps.iter().enumerate() {
        let label = format!("step {}", i + 1);
        if let Err(e) = run_step(&mut ctx, step, i, result).await {
            result.failures.push(format!("{label}: {e:#}"));
            break;
        }
    }
    drop(server);
    Ok(())
}

/// Objects the server must subscribe to before a case starts: `webhooks` for Klippy's state and
/// `print_stats` for job tracking (history, job queue).
const READY_SUBSCRIPTIONS: &[&str] = &["webhooks", "print_stats"];

/// Waits until the server reports Klippy ready and has subscribed to [`READY_SUBSCRIPTIONS`].
/// Moonraker reports `ready` slightly before it subscribes, and until then a change in Klippy
/// reaches nobody. With real Klipper the subscription can't be seen, so a short pause
/// stands in for it.
async fn wait_klippy_ready(ctx: &Ctx) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Ok(info)) = ctx.ws.call("server.info", None).await {
            let subscribed = ctx.fake.as_ref().is_none_or(|f| READY_SUBSCRIPTIONS.iter().all(|o| f.is_subscribed(o)));
            if info["klippy_state"] == "ready" && subscribed {
                if ctx.real.is_some() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            return Err(anyhow!("klippy not ready within {READY_TIMEOUT:?}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn run_step(ctx: &mut Ctx, step: &Step, index: usize, result: &mut RunResult) -> Result<()> {
    match step {
        Step::Call(c) => {
            if c.only.as_ref().is_some_and(|only| !only.contains(&ctx.transport)) {
                return Ok(());
            }
            let params = c.params.as_ref().map(|p| substitute(p, &ctx.vars));
            let deadline = c.retry_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
            loop {
                let outcome = call(ctx, &c.call, params.as_ref(), c.http.as_ref()).await?;
                let problems = check_outcome(&outcome, c.expect.as_ref(), c.expect_error.as_ref(), &ctx.vars);
                if problems.is_empty() || deadline.is_none_or(|d| Instant::now() > d) {
                    result.transcript.push(transcript_entry(index, &c.call, &outcome));
                    if !problems.is_empty() {
                        return Err(anyhow!("{}: {}", c.call, problems.join("; ")));
                    }
                    if let Ok(value) = &outcome {
                        for (name, path) in &c.save {
                            let path = path.as_str().context("save paths are strings")?;
                            let v = lookup(value, path).with_context(|| format!("save {name}: no value at {path}"))?;
                            ctx.vars.insert(name.clone(), v.clone());
                        }
                    }
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Step::Notification(n) => {
            let matcher = n.matches.as_ref().map(|m| substitute(m, &ctx.vars));
            let found = ctx
                .ws
                .wait_notification(Duration::from_millis(n.within_ms), |r| {
                    r.method == n.expect_notification
                        && matcher.as_ref().is_none_or(|m| mismatches(m, &r.params).is_empty())
                })
                .await;
            if found.is_none() {
                let seen: Vec<String> = ctx
                    .ws
                    .notifications()
                    .iter()
                    .filter(|r| r.method == n.expect_notification)
                    .map(|r| short(&r.params))
                    .collect();
                return Err(anyhow!(
                    "no matching {} within {}ms (got {})",
                    n.expect_notification,
                    n.within_ms,
                    if seen.is_empty() { "none".into() } else { seen.join(", ") }
                ));
            }
            Ok(())
        }
        Step::NoNotification(n) => {
            let found = ctx
                .ws
                .wait_notification(Duration::from_millis(n.within_ms), |r| {
                    r.method == n.expect_no_notification
                        && n.matches.as_ref().is_none_or(|m| mismatches(m, &r.params).is_empty())
                })
                .await;
            match found {
                Some(r) => Err(anyhow!("unexpected {}: {}", r.method, short(&r.params))),
                None => Ok(()),
            }
        }
        Step::Klippy(k) => {
            let fake = fake(ctx)?;
            match &k.klippy {
                KlippyAction::Update(patch) => fake.update_objects(patch),
                KlippyAction::GcodeOutput(line) => fake.gcode_output(line),
                KlippyAction::State { state, message } => fake.set_state(*state, message.as_deref()),
                KlippyAction::Restart => fake.restart(),
                KlippyAction::Disconnect => fake.disconnect_clients(),
                KlippyAction::RemoteMethod { method, params } => fake
                    .call_remote_method(method, params.clone().unwrap_or_else(|| json!({})))
                    .map_err(|e| anyhow!(e))?,
            }
            Ok(())
        }
        Step::KlippyRequest(k) => {
            // Only an observation: with real Klipper there's nothing to observe, so skip it.
            if ctx.real.is_some() {
                return Ok(());
            }
            let fake = fake(ctx)?;
            let expected = substitute(&k.expect_klippy_request, &ctx.vars);
            let deadline = Instant::now() + Duration::from_millis(k.within_ms);
            loop {
                if fake.requests().iter().any(|r| mismatches(&expected, r).is_empty()) {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    let methods: Vec<String> =
                        fake.requests().iter().map(|r| r["method"].as_str().unwrap_or("?").to_owned()).collect();
                    return Err(anyhow!("klippy never got {} (got: {})", short(&expected), methods.join(", ")));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Step::Raw(r) => {
            if ctx.transport != Transport::Ws {
                return Ok(());
            }
            let reply = ctx.ws.raw(&r.raw_ws, Duration::from_secs(5)).await?;
            if let Some(expect) = &r.expect {
                let problems = mismatches(expect, &reply);
                if !problems.is_empty() {
                    return Err(anyhow!("raw reply: {}", problems.join("; ")));
                }
            }
            Ok(())
        }
        Step::Upload(u) => {
            let content = match (&u.content, &u.fixture) {
                (Some(text), None) => text.clone().into_bytes(),
                (None, Some(name)) => read_fixture(&ctx.fixtures, name)?,
                _ => bail!("an upload step needs either `content` or `fixture`"),
            };
            let form = match substitute(&Value::Object(u.form.clone()), &ctx.vars) {
                Value::Object(m) => m,
                _ => unreachable!("substitute keeps the value's type"),
            };
            let outcome = ctx.http.upload(&u.upload, content, &form).await?;
            result.transcript.push(transcript_entry(index, "upload", &outcome));
            let problems = check_outcome(&outcome, u.expect.as_ref(), u.expect_error.as_ref(), &ctx.vars);
            if !problems.is_empty() {
                return Err(anyhow!("upload {}: {}", u.upload, problems.join("; ")));
            }
            Ok(())
        }
        Step::Download(d) => {
            let (status, content_type, body) = ctx.http.download(&d.download).await?;
            result.transcript.push(json!({"step": index + 1, "download": d.download, "status": status}));
            if status != d.expect_status {
                return Err(anyhow!("download {}: expected status {}, got {status}", d.download, d.expect_status));
            }
            if let Some(expected) = &d.expect_content_type
                && *expected != content_type
            {
                return Err(anyhow!("download {}: expected Content-Type {expected}, got {content_type}", d.download));
            }
            let expected = match (&d.expect_body, &d.expect_fixture) {
                (Some(text), _) => Some(text.clone().into_bytes()),
                (None, Some(name)) => Some(read_fixture(&ctx.fixtures, name)?),
                (None, None) => None,
            };
            if let Some(expected) = expected
                && expected != body
            {
                return Err(anyhow!(
                    "download {}: body differs ({} bytes, expected {}): {}",
                    d.download,
                    body.len(),
                    expected.len(),
                    String::from_utf8_lossy(&body[..body.len().min(120)])
                ));
            }
            Ok(())
        }
        Step::Sleep(s) => {
            tokio::time::sleep(Duration::from_millis(s.sleep_ms)).await;
            Ok(())
        }
    }
}

fn read_fixture(dir: &Path, name: &str) -> Result<Vec<u8>> {
    let path = dir.join(name);
    std::fs::read(&path).with_context(|| format!("reading fixture {}", path.display()))
}

fn fake(ctx: &Ctx) -> Result<&FakeKlippy> {
    if ctx.real.is_some() {
        anyhow::bail!("this step needs the fake Klippy; don't tag the case real-klippy");
    }
    ctx.fake.as_ref().context("case has `klippy: none`")
}

async fn call(
    ctx: &Ctx,
    method: &str,
    params: Option<&Value>,
    http: Option<&crate::case::HttpOverride>,
) -> Result<Outcome> {
    match (ctx.transport, http) {
        (Transport::Ws, _) => ctx.ws.call(method, params.cloned()).await,
        (Transport::Http, Some(h)) => {
            let body = h.body.as_ref().map(|b| substitute(b, &ctx.vars));
            ctx.http.request(&h.verb, &h.path, h.query.as_deref(), body.as_ref()).await
        }
        (Transport::Http, None) => ctx.http.call(method, params).await,
    }
}

fn check_outcome(
    outcome: &Outcome,
    expect: Option<&Value>,
    expect_error: Option<&Value>,
    vars: &Map<String, Value>,
) -> Vec<String> {
    match (outcome, expect_error) {
        (Err(e), Some(exp)) => {
            let actual = json!({"code": e.code, "message": e.message});
            mismatches(&substitute(exp, vars), &actual)
        }
        (Ok(v), Some(_)) => vec![format!("expected an error, got {}", short(v))],
        (Err(e), None) => vec![format!("error {}: {}", e.code, e.message)],
        (Ok(v), None) => expect.map(|exp| mismatches(&substitute(exp, vars), v)).unwrap_or_default(),
    }
}

fn transcript_entry(index: usize, method: &str, outcome: &Outcome) -> Value {
    match outcome {
        Ok(v) => json!({"step": index + 1, "call": method, "result": v}),
        Err(e) => json!({"step": index + 1, "call": method, "error": {"code": e.code, "message": e.message}}),
    }
}

/// Follows a dotted path (`a.b.0.c`) into a value.
fn lookup<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').filter(|p| !p.is_empty()).try_fold(value, |v, part| match v {
        Value::Object(m) => m.get(part),
        Value::Array(a) => part.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Replaces `${name}` in strings with saved values. A string that is exactly `${name}` takes the
/// saved value's type.
fn substitute(value: &Value, vars: &Map<String, Value>) -> Value {
    if vars.is_empty() {
        return value.clone();
    }
    match value {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}'))
                && let Some(v) = vars.get(name)
            {
                return v.clone();
            }
            let mut out = s.clone();
            for (k, v) in vars {
                let text = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("${{{k}}}"), &text);
            }
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| substitute(i, vars)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), substitute(v, vars))).collect()),
        other => other.clone(),
    }
}
