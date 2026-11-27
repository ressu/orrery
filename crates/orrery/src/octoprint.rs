//! `[octoprint_compat]`: the part of OctoPrint's API that slicers use to send a print, after
//! Moonraker's `components/octoprint_compat.py` (docs/design/2026-11-27-octoprint-compat.md).
//!
//! PrusaSlicer, and the slicers built from it, test the connection with `GET /api/version` and
//! upload with `POST /api/files/local`. Cura's OctoPrint plugin uses the rest. The paths answer
//! over HTTP only, without Moonraker's `{"result": …}`, and each has a role
//! ([`crate::auth`]). The upload is Orrery's own, in [`crate::files::http`].
//!
//! It differs from Moonraker's in three ways: the version says Orrery, `/api/currentuser` names
//! the caller and its role, and there's no webcam. The UFP plugin is reported when Orrery
//! reads UFP files ([`crate::files::readers`]), as any format follows its reader, so
//! Moonraker's `enable_ufp` does nothing (user, 2026-11-27).

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::auth::Role;
use crate::config::Config;
use crate::klippy::KlippyState;
use crate::server::App;

/// Where OctoPrint's clients find an uploaded file, to start printing it.
pub const FILES_PREFIX: &str = "/api/files/moonraker/";
/// The endpoint that answers under [`FILES_PREFIX`], with the rest of the path as
/// `relative_path`.
pub const FILES_ENDPOINT: &str = "/api/files/moonraker";
/// The OctoPrint version Moonraker reports.
const OCTO_VERSION: &str = "1.5.0";
/// Moonraker's webcam options, which Orrery doesn't use.
const WEBCAM_OPTIONS: &[&str] = &["flip_h", "flip_v", "rotate_90", "stream_url", "webcam_enabled"];

/// OctoPrint compatibility, turned on by `[octoprint_compat]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OctoPrint {
    /// Whether Cura hears that OctoPrint's UFP plugin is installed, and then sends `.ufp` files.
    ufp: bool,
}

impl OctoPrint {
    /// `None` without `[octoprint_compat]`, and warnings for the options Orrery ignores.
    /// `reads_ufp` is whether Orrery prints `.ufp` files.
    pub fn new(config: &Config, reads_ufp: bool) -> anyhow::Result<(Option<Self>, Vec<String>)> {
        if config.section_json("octoprint_compat").is_none() {
            return Ok((None, Vec::new()));
        }
        let section = config.section("octoprint_compat");
        let mut warnings = Vec::new();
        // Read as Moonraker reads them, so that server.config lists them.
        let enable_ufp = section.get_bool("enable_ufp", true)?;
        let set: Vec<&str> = WEBCAM_OPTIONS.iter().copied().filter(|o| section.has(o)).collect();
        for option in ["flip_h", "flip_v", "rotate_90"] {
            section.get_bool(option, false)?;
        }
        section.get_str("stream_url", "/webcam/?action=stream");
        section.get_bool("webcam_enabled", true)?;
        // Only when it asks for what doesn't happen: set as Orrery does it anyway, it's harmless.
        if section.has("enable_ufp") && enable_ufp != reads_ufp {
            warnings.push(if reads_ufp {
                "[octoprint_compat] enable_ufp: ignored, so Cura still sends UFP files, which Orrery prints. UFP \
                 files follow their reader: for gcode from Cura, turn the reader off with [reader.ufp] enabled = \
                 false in orrery.toml."
                    .to_owned()
            } else {
                "[octoprint_compat] enable_ufp: ignored, since [reader.ufp] in orrery.toml has enabled = false, so \
                 Orrery doesn't tell Cura that it takes UFP files. Cura uploads gcode instead."
                    .to_owned()
            });
        }
        if !set.is_empty() {
            warnings.push(format!(
                "[octoprint_compat] {}: Orrery doesn't report a webcam to OctoPrint's clients, so {} ignored.",
                set.join(", "),
                if set.len() == 1 { "it's" } else { "they're" }
            ));
        }
        Ok((Some(Self { ufp: reads_ufp }), warnings))
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    let get = &[RequestType::Get];
    let post = &[RequestType::Post];
    r.register("/api/version", get, Transports::HttpBare, handler(version));
    r.register("/api/server", get, Transports::HttpBare, handler(server));
    r.register("/api/login", post, Transports::HttpBare, handler(current_user));
    r.register("/api/currentuser", get, Transports::HttpBare, handler(current_user));
    r.register("/api/settings", get, Transports::HttpBare, handler(settings));
    r.register("/api/job", get, Transports::HttpBare, handler(job));
    r.register("/api/printer", get, Transports::HttpBare, handler(printer));
    r.register("/api/printer/command", post, Transports::HttpBare, handler(command));
    r.register("/api/printerprofiles", get, Transports::HttpBare, handler(printer_profiles));
    r.register(FILES_ENDPOINT, post, Transports::HttpBare, handler(select_file));
}

async fn version(_app: Arc<App>, _req: Request) -> ApiResult {
    Ok(json!({
        "server": OCTO_VERSION,
        "api": "0.1",
        "text": format!("OctoPrint (Orrery {})", orrery_build::VERSION),
    }))
}

async fn server(app: Arc<App>, _req: Request) -> ApiResult {
    let ready = app.klippy.state().0 == KlippyState::Ready;
    Ok(json!({"server": OCTO_VERSION, "safemode": if ready { Value::Null } else { json!("settings") }}))
}

/// The caller, in OctoPrint's terms: the user or the key, how it got in, and whether its role
/// is `admin`. Moonraker answers that everyone is `_api`, an admin.
async fn current_user(_app: Arc<App>, req: Request) -> ApiResult {
    Ok(user_json(&req.access))
}

fn user_json(access: &crate::auth::Access) -> Value {
    // OctoPrint's mechanisms: `password` for a login, `http` for a proxy's header, `apikey` for
    // a key, and `autologin` for a trusted network. A trusted network's caller is named by its
    // own address, which tells it nothing new. Cura asks again while it has no name.
    let (name, mechanism) = match (&access.user, &access.key) {
        (Some(user), _) if access.from_proxy => (json!(user), "http"),
        (Some(user), _) => (json!(user), "password"),
        (None, Some(key)) => (json!(key), "apikey"),
        (None, None) if !access.client.is_empty() => (json!(access.client), "autologin"),
        (None, None) => (json!("trusted user"), "autologin"),
    };
    let groups = match access.role {
        Some(Role::Admin) => json!(["admins", "users"]),
        Some(Role::Operator) => json!(["users"]),
        _ => json!(["readonly"]),
    };
    json!({
        "_is_external_client": false,
        "_login_mechanism": mechanism,
        "name": name,
        "active": true,
        "user": true,
        "admin": access.role == Some(Role::Admin),
        "apikey": null,
        "permissions": [],
        "groups": groups,
    })
}

/// No SD card and no webcam. The UFP plugin, as Moonraker reports it, has Cura send `.ufp`
/// files, for their picture.
async fn settings(app: Arc<App>, _req: Request) -> ApiResult {
    let plugins = if app.octoprint.as_ref().is_some_and(|o| o.ufp) {
        json!({"UltimakerFormatPackage": {
            "align_inline_thumbnail": false,
            "inline_thumbnail": false,
            "inline_thumbnail_align_value": "left",
            "inline_thumbnail_scale_value": "50",
            "installed": true,
            "installed_version": "0.2.2",
            "scale_inline_thumbnail": false,
            "state_panel_thumbnail": true,
        }})
    } else {
        json!({})
    };
    Ok(json!({
        "plugins": plugins,
        "feature": {"sdSupport": false, "temperatureGraph": false},
        "webcam": {"flipH": false, "flipV": false, "rotate90": false, "streamUrl": "", "webcamEnabled": false},
    }))
}

async fn job(app: Arc<App>, _req: Request) -> ApiResult {
    Ok(json!({
        "job": {"file": {"name": null}, "estimatedPrintTime": null, "filament": {"length": null}, "user": null},
        "progress": {
            "completion": null,
            "filepos": null,
            "printTime": null,
            "printTimeLeft": null,
            "printTimeOrigin": null,
        },
        "state": printer_state(&app),
    }))
}

/// The printer's state in OctoPrint's words, as Moonraker's `printer_state`.
fn printer_state(app: &App) -> &'static str {
    match app.klippy.state().0 {
        KlippyState::Disconnected | KlippyState::Startup => "Offline",
        KlippyState::Error | KlippyState::Shutdown => "Error",
        KlippyState::Ready => {
            let stats = app.job_state.last_stats();
            match stats.get("state").and_then(Value::as_str).unwrap_or("standby") {
                "standby" | "complete" => "Operational",
                "printing" => "Printing",
                "paused" => "Paused",
                _ => "Error",
            }
        }
    }
}

async fn printer(app: Arc<App>, _req: Request) -> ApiResult {
    let state = printer_state(&app);
    let sensors = sensors(&app).await;
    Ok(json!({
        "temperature": temperatures(&sensors),
        "state": {
            "text": state,
            "flags": {
                "operational": !matches!(state, "Error" | "Offline"),
                "paused": state == "Paused",
                "printing": state == "Printing",
                "cancelling": false,
                "pausing": false,
                "error": state == "Error",
                "ready": state == "Operational",
                "closedOrError": matches!(state, "Error" | "Offline"),
            },
        },
    }))
}

/// Klipper's temperature sensors, `heaters.available_sensors`, with their state. Empty while
/// Klipper isn't ready.
async fn sensors(app: &App) -> Vec<(String, Map<String, Value>)> {
    if app.klippy.state().0 != KlippyState::Ready {
        return Vec::new();
    }
    let query = |objects: Value| app.klippy.request("objects/query", json!({"objects": objects}), None);
    let names: Vec<String> = match query(json!({"heaters": ["available_sensors"]})).await {
        Ok(r) => r["status"]["heaters"]["available_sensors"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default(),
        Err(e) => {
            tracing::info!("Error reading the heaters for OctoPrint's API: {e}");
            return Vec::new();
        }
    };
    if names.is_empty() {
        return Vec::new();
    }
    let objects: Map<String, Value> = names.iter().map(|n| (n.clone(), Value::Null)).collect();
    let status = query(Value::Object(objects)).await.map(|r| r["status"].clone()).unwrap_or_default();
    names.into_iter().map(|n| (n.clone(), status[&n].as_object().cloned().unwrap_or_default())).collect()
}

/// The bed and each extruder, as `bed` and `tool<n>`, as Moonraker's `printer_temps`.
fn temperatures(sensors: &[(String, Map<String, Value>)]) -> Value {
    let mut temps = Map::new();
    for (name, data) in sensors {
        let key = if let Some(n) = name.strip_prefix("extruder") {
            format!("tool{}", n.parse::<u32>().unwrap_or(0))
        } else if name == "heater_bed" {
            "bed".to_owned()
        } else {
            continue;
        };
        let actual = data.get("temperature").and_then(Value::as_f64).unwrap_or(0.0);
        let target = data.get("target").and_then(Value::as_f64).unwrap_or(0.0);
        temps.insert(key, json!({"actual": (actual * 100.0).round() / 100.0, "offset": 0, "target": target}));
    }
    Value::Object(temps)
}

/// Runs each gcode command. A command that fails is logged, and the answer is still `{}`, as in
/// Moonraker and OctoPrint.
async fn command(app: Arc<App>, req: Request) -> ApiResult {
    let commands = if req.args.get("commands").is_some() { req.args.list("commands")? } else { Vec::new() };
    for command in commands {
        tracing::info!("Executing GCode: {command}");
        if let Err(e) = crate::klippy::run_gcode(&app, &command).await {
            tracing::info!("Error executing GCode {command}: {e}");
        }
    }
    Ok(json!({}))
}

async fn printer_profiles(app: Arc<App>, _req: Request) -> ApiResult {
    let sensors = sensors(&app).await;
    let has = |name: &str| sensors.iter().any(|(n, _)| n == name);
    let axis = |speed: f64| json!({"speed": speed, "inverted": false});
    Ok(json!({
        "profiles": {
            "_default": {
                "id": "_default",
                "name": "Default",
                "color": "default",
                "model": "Default",
                "default": true,
                "current": true,
                "heatedBed": has("heater_bed"),
                "heatedChamber": has("chamber"),
                "axes": {"x": axis(6000.0), "y": axis(6000.0), "z": axis(6000.0), "e": axis(300.0)},
            }
        }
    }))
}

/// `POST /api/files/moonraker/<root>/<path>` with `select`: starts printing an uploaded file,
/// or queues it, as Cura does with an upload's `Location`. Selecting without printing does
/// nothing, since Moonraker has no selected file.
async fn select_file(app: Arc<App>, req: Request) -> ApiResult {
    let command = req.args.str("command")?;
    let relative = req.args.str("relative_path")?;
    if command != "select" {
        return Err(ApiError::bad_request(format!("Unsupported Command: {command}")));
    }
    let Some((root, filename)) = relative.trim_matches('/').split_once('/') else {
        return Err(ApiError::bad_request(format!("No file in {relative}")));
    };
    if !req.args.opt_bool("print")?.unwrap_or(false) {
        return Ok(Value::Null);
    }
    if root != "gcodes" {
        return Err(ApiError::bad_request("File must be located in the 'gcodes' root"));
    }
    let exists = app.files.root("gcodes").is_some_and(|r| {
        let path = crate::files::normalize(&r.path.join(filename));
        path.starts_with(&r.path) && path.exists()
    });
    if !exists {
        return Err(ApiError::bad_request("File does not exist"));
    }
    let state = match app.klippy.request("objects/query", json!({"objects": {"print_stats": ["state"]}}), None).await {
        Ok(r) => r["status"]["print_stats"]["state"].as_str().unwrap_or("not_avail").to_owned(),
        Err(_) => "not_avail".to_owned(),
    };
    let mut started = false;
    if !matches!(state.as_str(), "printing" | "paused" | "not_avail") {
        started = crate::klippy::start_print(&app, filename).await.is_ok();
        if started {
            tracing::debug!("Job '{filename}' started via OctoPrint API");
        }
    }
    if !started {
        if !app.files.queue_gcodes {
            return Err(ApiError::new(409, "Conflict"));
        }
        crate::job_queue::queue_job(&app, vec![filename.to_owned()], false, false).await?;
        tracing::debug!("Job '{filename}' queued via OctoPrint API");
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moonraker.conf");
        std::fs::write(&path, text).unwrap();
        Config::load(&path).unwrap()
    }

    #[test]
    fn the_section_turns_it_on_and_ignored_options_warn() {
        assert_eq!(OctoPrint::new(&config("[server]\n"), true).unwrap(), (None, vec![]));
        assert_eq!(
            OctoPrint::new(&config("[octoprint_compat]\n"), true).unwrap(),
            (Some(OctoPrint { ufp: true }), vec![])
        );
        // With UFP files turned off, the plugin isn't reported, and asking for it warns.
        assert_eq!(
            OctoPrint::new(&config("[octoprint_compat]\n"), false).unwrap(),
            (Some(OctoPrint { ufp: false }), vec![])
        );
        let (on, warnings) =
            OctoPrint::new(&config("[octoprint_compat]\nenable_ufp: True\nstream_url: /cam\nflip_h: True\n"), false)
                .unwrap();
        assert_eq!(on, Some(OctoPrint { ufp: false }));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("enable_ufp"));
        assert!(warnings[1].starts_with("[octoprint_compat] flip_h, stream_url:"), "{}", warnings[1]);
        // enable_ufp is ignored: the reader decides, and setting it otherwise warns.
        let (on, warnings) = OctoPrint::new(&config("[octoprint_compat]\nenable_ufp: False\n"), true).unwrap();
        assert_eq!(on, Some(OctoPrint { ufp: true }));
        assert!(warnings[0].contains("[reader.ufp] enabled = false"), "{warnings:?}");
        assert_eq!(
            OctoPrint::new(&config("[octoprint_compat]\nenable_ufp: True\n"), true).unwrap(),
            (Some(OctoPrint { ufp: true }), vec![])
        );
        assert!(OctoPrint::new(&config("[octoprint_compat]\nenable_ufp: maybe\n"), true).is_err());
    }

    #[test]
    fn temperatures_name_the_bed_and_the_tools() {
        let sensor = |t: f64, target: f64| json!({"temperature": t, "target": target}).as_object().unwrap().clone();
        let sensors = vec![
            ("extruder".to_owned(), sensor(210.456, 210.0)),
            ("extruder1".to_owned(), sensor(25.0, 0.0)),
            ("heater_bed".to_owned(), sensor(60.0, 60.0)),
            ("temperature_sensor chamber".to_owned(), sensor(30.0, 0.0)),
        ];
        assert_eq!(
            temperatures(&sensors),
            json!({
                "tool0": {"actual": 210.46, "offset": 0, "target": 210.0},
                "tool1": {"actual": 25.0, "offset": 0, "target": 0.0},
                "bed": {"actual": 60.0, "offset": 0, "target": 60.0},
            })
        );
    }

    #[test]
    fn the_user_is_the_caller() {
        let key =
            crate::auth::Access { role: Some(Role::Operator), key: Some("prusaslicer".into()), ..Default::default() };
        let user = user_json(&key);
        assert_eq!(
            (&user["name"], &user["_login_mechanism"], &user["admin"], &user["groups"]),
            (&json!("prusaslicer"), &json!("apikey"), &json!(false), &json!(["users"]))
        );
        let network = crate::auth::Access {
            role: Some(Role::Admin),
            trusted: true,
            client: "192.168.1.157".into(),
            ..Default::default()
        };
        let user = user_json(&network);
        assert_eq!(
            (&user["name"], &user["_login_mechanism"], &user["admin"]),
            (&json!("192.168.1.157"), &json!("autologin"), &json!(true))
        );
        let unnamed = crate::auth::Access { role: Some(Role::Viewer), trusted: true, ..Default::default() };
        let user = user_json(&unnamed);
        assert_eq!((&user["name"], &user["groups"]), (&json!("trusted user"), &json!(["readonly"])));
    }
}
