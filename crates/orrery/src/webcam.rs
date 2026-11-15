//! Webcams, after Moonraker's `webcam.py`: `server.webcams.*` and `notify_webcams_changed`.
//!
//! Webcams come from `[webcam <name>]` config sections and from the `webcams` database
//! namespace, where clients save them. Config webcams can't be changed or deleted through the
//! API. Each webcam has a uid: config webcams get a UUID5 from the instance id, so the uid stays
//! the same across restarts, and saved webcams get a random UUID4.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::api::{ApiError, ApiResult, Args, Request, RequestType, Transports, handler};
use crate::config::Config;
use crate::server::{App, Paths};

const NAMESPACE: &str = "webcams";

#[derive(Debug, Clone, PartialEq)]
struct WebCam {
    name: String,
    enabled: bool,
    icon: String,
    aspect_ratio: String,
    target_fps: i64,
    target_fps_idle: i64,
    location: String,
    service: String,
    stream_url: String,
    snapshot_url: String,
    flip_horizontal: bool,
    flip_vertical: bool,
    rotation: i64,
    source: &'static str,
    extra_data: Value,
    uid: String,
}

impl WebCam {
    fn validate(self) -> Result<Self, ApiError> {
        if ![0, 90, 180, 270].contains(&self.rotation) {
            return Err(ApiError::bad_request(format!("Invalid value for 'rotation': {}", self.rotation)));
        }
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        match self.aspect_ratio.split_once(':') {
            Some((a, b)) if digits(a) && digits(b) => Ok(self),
            _ => Err(ApiError::bad_request(format!("Invalid value for 'aspect_ratio': {}", self.aspect_ratio))),
        }
    }

    /// The webcam as Moonraker's `as_dict` gives it.
    fn as_dict(&self) -> Value {
        json!({
            "name": self.name,
            "enabled": self.enabled,
            "icon": self.icon,
            "aspect_ratio": self.aspect_ratio,
            "target_fps": self.target_fps,
            "target_fps_idle": self.target_fps_idle,
            "location": self.location,
            "service": self.service,
            "stream_url": self.stream_url,
            "snapshot_url": self.snapshot_url,
            "flip_horizontal": self.flip_horizontal,
            "flip_vertical": self.flip_vertical,
            "rotation": self.rotation,
            "source": self.source,
            "extra_data": self.extra_data,
            "uid": self.uid,
        })
    }

    /// The record saved in the `webcams` namespace, with the field names frontends used before
    /// Moonraker managed webcams.
    fn db_record(&self) -> Value {
        json!({
            "name": self.name,
            "service": self.service,
            "targetFps": self.target_fps,
            "urlStream": self.stream_url,
            "urlSnapshot": self.snapshot_url,
            "flipX": self.flip_horizontal,
            "flipY": self.flip_vertical,
            "enabled": self.enabled,
            "targetFpsIdle": self.target_fps_idle,
            "aspectRatio": self.aspect_ratio,
            "icon": self.icon,
            "location": self.location,
            "rotation": self.rotation,
            "extra_data": self.extra_data,
        })
    }

    fn from_request(args: &Args, uid: String) -> Result<Self, ApiError> {
        let str_or = |k: &str, d: &str| Ok::<_, ApiError>(args.opt_str(k)?.unwrap_or_else(|| d.to_owned()));
        Self {
            name: args.str("name")?,
            enabled: args.opt_bool("enabled")?.unwrap_or(true),
            icon: str_or("icon", "mdiWebcam")?,
            aspect_ratio: str_or("aspect_ratio", "4:3")?,
            target_fps: args.opt_int("target_fps")?.unwrap_or(15),
            target_fps_idle: args.opt_int("target_fps_idle")?.unwrap_or(5),
            location: str_or("location", "printer")?,
            service: str_or("service", "mjpegstreamer")?,
            stream_url: args.str("stream_url")?,
            snapshot_url: str_or("snapshot_url", "")?,
            flip_horizontal: args.opt_bool("flip_horizontal")?.unwrap_or(false),
            flip_vertical: args.opt_bool("flip_vertical")?.unwrap_or(false),
            rotation: args.opt_int("rotation")?.unwrap_or(0),
            source: "database",
            extra_data: args.get("extra_data").cloned().unwrap_or_else(|| json!({})),
            uid,
        }
        .validate()
    }

    /// A saved webcam, converted the way Python's `str()`, `int()`, and `bool()` would.
    fn from_database(uid: &str, data: &Value) -> Result<Self, ApiError> {
        let args = Args(data.as_object().cloned().unwrap_or_default());
        let s = |k: &str, d: &str| Ok::<_, ApiError>(args.opt_str(k)?.unwrap_or_else(|| d.to_owned()));
        let i = |k: &str, d: i64| Ok::<_, ApiError>(args.opt_int(k)?.unwrap_or(d));
        let b = |k: &str, d: bool| args.get(k).map_or(d, truthy);
        let rotation = match args.get("rotation") {
            Some(_) => i("rotation", 0)?,
            None => i("rotate", 0)?,
        };
        Self {
            name: args.str("name")?,
            enabled: b("enabled", true),
            icon: s("icon", "mdiWebcam")?,
            aspect_ratio: s("aspectRatio", "4:3")?,
            target_fps: i("targetFps", 15)?,
            target_fps_idle: i("targetFpsIdle", 5)?,
            location: s("location", "printer")?,
            service: s("service", "mjpegstreamer")?,
            stream_url: s("urlStream", "")?,
            snapshot_url: s("urlSnapshot", "")?,
            flip_horizontal: b("flipX", false),
            flip_vertical: b("flipY", false),
            rotation,
            source: "database",
            extra_data: args.get("extra_data").cloned().unwrap_or_else(|| json!({})),
            uid: uid.to_owned(),
        }
        .validate()
    }

    fn from_config(config: &Config, section_name: &str, instance: &uuid::Uuid) -> Result<Self> {
        let name = section_name.split_once(char::is_whitespace).map_or(section_name, |(_, n)| n.trim()).to_owned();
        let section = config.section(section_name);
        let Some(stream_url) = section.get_opt_str("stream_url") else {
            bail!("[{section_name}]: option 'stream_url' is required");
        };
        let cam = Self {
            enabled: section.get_bool("enabled", true)?,
            icon: section.get_str("icon", "mdiWebcam"),
            aspect_ratio: section.get_str("aspect_ratio", "4:3"),
            target_fps: section.get_int("target_fps", 15)?,
            target_fps_idle: section.get_int("target_fps_idle", 5)?,
            location: section.get_str("location", "printer"),
            service: section.get_str("service", "mjpegstreamer"),
            stream_url,
            snapshot_url: section.get_str("snapshot_url", ""),
            flip_horizontal: section.get_bool("flip_horizontal", false)?,
            flip_vertical: section.get_bool("flip_vertical", false)?,
            rotation: section.get_int("rotation", 0)?,
            source: "config",
            extra_data: json!({}),
            uid: uuid::Uuid::new_v5(instance, format!("moonraker.webcam.{name}").as_bytes()).to_string(),
            name,
        };
        cam.validate().map_err(|e| anyhow::anyhow!("[{section_name}]: {e}"))
    }

    /// Moonraker's `update`: sets each field the request names, except `source` and `uid`.
    fn update(&mut self, args: &Args) -> Result<(), ApiError> {
        for key in args.0.keys() {
            match key.as_str() {
                "name" => self.name = args.str(key)?,
                "enabled" => self.enabled = args.bool(key)?,
                "icon" => self.icon = args.str(key)?,
                "aspect_ratio" => self.aspect_ratio = args.str(key)?,
                "target_fps" => self.target_fps = args.int(key)?,
                "target_fps_idle" => self.target_fps_idle = args.int(key)?,
                "location" => self.location = args.str(key)?,
                "service" => self.service = args.str(key)?,
                "stream_url" => self.stream_url = args.str(key)?,
                "snapshot_url" => self.snapshot_url = args.str(key)?,
                "flip_horizontal" => self.flip_horizontal = args.bool(key)?,
                "flip_vertical" => self.flip_vertical = args.bool(key)?,
                "rotation" => self.rotation = args.int(key)?,
                "extra_data" => self.extra_data = args.0[key].clone(),
                _ => {}
            }
        }
        Ok(())
    }
}

/// Python truthiness.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub struct Webcams {
    /// In the order Moonraker keeps them: config webcams first, then saved ones.
    cams: Mutex<Vec<WebCam>>,
}

impl Webcams {
    pub fn new(config: &Config, paths: &Paths) -> Result<Self> {
        let instance = instance_uuid(paths)?;
        let mut cams: Vec<WebCam> = Vec::new();
        for section in config.section_names("webcam ") {
            let cam = WebCam::from_config(config, &section, &instance)?;
            cams.retain(|c| c.name != cam.name);
            cams.push(cam);
        }
        Ok(Self { cams: Mutex::new(cams) })
    }

    fn cams(&self) -> std::sync::MutexGuard<'_, Vec<WebCam>> {
        self.cams.lock().expect("webcams lock")
    }

    fn list(&self) -> Value {
        Value::Array(self.cams().iter().map(WebCam::as_dict).collect())
    }

    fn guaranteed_uuid(&self) -> String {
        let cams = self.cams();
        loop {
            let uid = uuid::Uuid::new_v4().to_string();
            if !cams.iter().any(|c| c.uid == uid) {
                return uid;
            }
        }
    }
}

/// Moonraker's instance id, in `<data>/.moonraker.uuid`. orrery reads the same file, so config
/// webcams keep the uids Moonraker gave them.
fn instance_uuid(paths: &Paths) -> Result<uuid::Uuid> {
    let path = paths.data.join(".moonraker.uuid");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(id) = uuid::Uuid::parse_str(text.trim())
    {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4();
    std::fs::write(&path, id.simple().to_string()).with_context(|| format!("can't write {}", path.display()))?;
    Ok(id)
}

pub fn register_endpoints(app: &Arc<App>) {
    use RequestType::{Delete, Get, Post};
    let mut r = app.registry.write().expect("registry lock");
    r.register("/server/webcams/list", &[Get], Transports::All, handler(|app, _| list(app)));
    r.register("/server/webcams/item", &[Get, Post, Delete], Transports::All, handler(item));
    r.register("/server/webcams/test", &[Post], Transports::All, handler(test));
}

/// Adds the webcams saved in the database. A saved webcam with the name of a config webcam is
/// ignored.
pub fn start(app: &Arc<App>) {
    let saved = match app.database.get_item(NAMESPACE, None) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut notes = Vec::new();
    for (uid, data) in saved {
        let mut cam = match WebCam::from_database(&uid, &data) {
            Ok(cam) => cam,
            Err(e) => {
                tracing::warn!("Failed to process webcam from db: {e}");
                continue;
            }
        };
        let (uid_taken, name_taken) = {
            let cams = app.webcams.cams();
            (cams.iter().any(|c| c.uid == uid), cams.iter().any(|c| c.name == cam.name))
        };
        if uid_taken {
            // Unlikely but possible: a random uid equal to one made for a config webcam.
            let _ = app.database.delete_item(NAMESPACE, std::slice::from_ref(&uid));
            cam.uid = app.webcams.guaranteed_uuid();
            if let Err(e) = app.database.insert_item(NAMESPACE, std::slice::from_ref(&cam.uid), cam.db_record()) {
                tracing::warn!("Unable to save webcam {}: {e}", cam.name);
            }
            notes.push(format!("Detected webcam UID collision: {uid}"));
        }
        if name_taken {
            notes.push(format!(
                "Detected webcam name collision: {}, uuid: {uid}.  This camera will be ignored.",
                cam.name
            ));
            continue;
        }
        app.webcams.cams().push(cam);
    }
    if !notes.is_empty() {
        tracing::info!("{}", notes.join("\n"));
    }
}

async fn list(app: Arc<App>) -> ApiResult {
    Ok(json!({"webcams": app.webcams.list()}))
}

/// Moonraker's `_lookup_camera`: by `uid` when given, otherwise by `name`.
fn lookup(app: &App, args: &Args, required: bool) -> Result<Option<WebCam>, ApiError> {
    let cams = app.webcams.cams();
    if args.get("uid").is_some() {
        let uid = args.str("uid")?;
        return match cams.iter().find(|c| c.uid == uid) {
            Some(c) => Ok(Some(c.clone())),
            None => Err(ApiError::not_found(format!("Webcam with UID {uid} not found"))),
        };
    }
    let name = args.str("name")?;
    match cams.iter().find(|c| c.name == name) {
        Some(c) => Ok(Some(c.clone())),
        None if required => Err(ApiError::not_found(format!("Webcam {name} not found"))),
        None => Ok(None),
    }
}

async fn item(app: Arc<App>, req: Request) -> ApiResult {
    let args = &req.args;
    let webcam = lookup(&app, args, req.kind != RequestType::Post)?;
    let data = match req.kind {
        RequestType::Get => webcam.expect("required").as_dict(),
        RequestType::Post => {
            let cam = match webcam {
                Some(mut cam) => {
                    if cam.source == "config" {
                        return Err(ApiError::bad_request(format!(
                            "Cannot overwrite webcam '{}' sourced from Moonraker configuration",
                            cam.name
                        )));
                    }
                    let old_name = cam.name.clone();
                    if let Some(new_name) = args.opt_str("name")?
                        && new_name != old_name
                    {
                        if app.webcams.cams().iter().any(|c| c.name == new_name) {
                            return Err(ApiError::bad_request(format!(
                                "Cannot rename webcam from '{old_name}' to '{new_name}'.  Webcam with requested \
                                 name '{new_name}' already exists."
                            )));
                        }
                        app.webcams.cams().retain(|c| c.name != old_name);
                    }
                    cam.update(args)?;
                    cam
                }
                None => WebCam::from_request(args, app.webcams.guaranteed_uuid())?,
            };
            {
                let mut cams = app.webcams.cams();
                match cams.iter_mut().find(|c| c.name == cam.name) {
                    Some(existing) => *existing = cam.clone(),
                    None => cams.push(cam.clone()),
                }
            }
            app.database.insert_item(NAMESPACE, std::slice::from_ref(&cam.uid), cam.db_record())?;
            cam.as_dict()
        }
        RequestType::Delete => {
            let cam = webcam.expect("required");
            if cam.source == "config" {
                return Err(ApiError::bad_request(format!(
                    "Cannot delete webcam '{}' sourced from Moonraker configuration",
                    cam.name
                )));
            }
            app.webcams.cams().retain(|c| c.name != cam.name);
            let _ = app.database.delete_item(NAMESPACE, std::slice::from_ref(&cam.uid));
            cam.as_dict()
        }
    };
    if req.kind != RequestType::Get {
        app.notify("webcams_changed", vec![json!({"webcams": app.webcams.list()})]);
    }
    Ok(json!({"webcam": data}))
}

/// Moonraker's `_handle_webcam_test`: the full URLs, and whether the snapshot URL answers.
async fn test(app: Arc<App>, req: Request) -> ApiResult {
    let cam = lookup(&app, &req.args, true)?.expect("required");
    let snapshot_url = full_url(&app, &cam.snapshot_url).await;
    let stream_url = full_url(&app, &cam.stream_url).await;
    let reachable = snapshot_url.starts_with("http") && url_reachable(&snapshot_url).await;
    Ok(json!({
        "name": cam.name,
        "snapshot_reachable": reachable,
        "snapshot_url": snapshot_url,
        "stream_url": stream_url,
    }))
}

/// Moonraker's `_get_url` with `convert_local`: a partial URL is on this host, and a URL for a
/// local service becomes its loopback address.
async fn full_url(app: &Arc<App>, url: &str) -> String {
    if url.is_empty() {
        tracing::info!("Error Processing url: Empty URL Provided");
        return String::new();
    }
    let url = if Regex::new(r"^\w+://[^/]+").expect("valid regex").is_match(url) {
        url.to_owned()
    } else {
        format!("{}/{}", default_host(), url.trim_start_matches('/'))
    };
    convert_local(app, &url).await
}

fn default_host() -> String {
    match crate::machine::public_ip() {
        Some(std::net::IpAddr::V6(ip)) => format!("http://[{ip}]"),
        Some(ip) => format!("http://{ip}"),
        None => "http://127.0.0.1".to_owned(),
    }
}

async fn convert_local(app: &Arc<App>, url: &str) -> String {
    let parts = Regex::new(r"^(\w+)://([^/]+)(/.*)?").expect("valid regex");
    let Some(c) = parts.captures(url) else { return url.to_owned() };
    let scheme = c[1].to_owned();
    let host_port = c[2].to_owned();
    let fragment = c.get(3).map_or("", |m| m.as_str()).to_owned();
    let addr_re = if host_port.starts_with('[') { r"^\[(.+)\](:\d+)?" } else { r"^([^:]+)(:\d+)?" };
    let Some(a) = Regex::new(addr_re).expect("valid regex").captures(&host_port) else { return url.to_owned() };
    let host = a[1].to_owned();
    let port = match a.get(2) {
        Some(p) => p.as_str().trim_start_matches(':').to_owned(),
        None => match scheme.as_str() {
            "http" => "80".to_owned(),
            "https" => "443".to_owned(),
            "rtsp" => "554".to_owned(),
            _ => return url.to_owned(),
        },
    };
    let resolved = match tokio::net::lookup_host((host.as_str(), port.parse().unwrap_or(0))).await {
        Ok(mut addrs) => addrs.next().map(|a| a.ip()),
        Err(_) => host.parse().ok(),
    };
    let Some(ip) = resolved else { return url.to_owned() };
    if ip.is_loopback() || !crate::machine::local_ips(app).contains(&ip.to_string()) {
        return url.to_owned();
    }
    let Ok(out) = tokio::process::Command::new("ss").arg("-ltn").output().await else { return url.to_owned() };
    for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let Some((laddr, lport)) = fields[3].rsplit_once(':') else { continue };
        if lport == port {
            match laddr {
                "[::]" => return format!("{scheme}://[::1]:{port}{fragment}"),
                "0.0.0.0" => return format!("{scheme}://127.0.0.1:{port}{fragment}"),
                _ => {}
            }
        }
    }
    url.to_owned()
}

/// A GET with one-second timeouts. orrery has no TLS, so an `https` URL is never reachable.
async fn url_reachable(url: &str) -> bool {
    let Some(c) = Regex::new(r"^http://([^/:]+|\[[^\]]+\])(?::(\d+))?(/.*)?$").expect("valid regex").captures(url)
    else {
        return false;
    };
    let host = c[1].trim_matches(['[', ']']).to_owned();
    let port: u16 = c.get(2).and_then(|p| p.as_str().parse().ok()).unwrap_or(80);
    let path = c.get(3).map_or("/", |m| m.as_str());
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect((host.as_str(), port)).await.ok()?;
        let request = format!("GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", &c[1]);
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut head = [0u8; 32];
        let n = stream.read(&mut head).await.ok()?;
        let status: u16 = std::str::from_utf8(&head[..n]).ok()?.split_whitespace().nth(1)?.parse().ok()?;
        Some(status < 400)
    };
    tokio::time::timeout(Duration::from_secs(2), attempt).await.ok().flatten().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_webcam_uid_matches_moonraker() {
        let config = Config::from_str("[webcam front]\nstream_url: /webcam/?action=stream\n").unwrap();
        let instance = uuid::Uuid::parse_str("8d3c9b1f6a2e4f0b9c7d5e3a1b2c4d6e").unwrap();
        let cam = WebCam::from_config(&config, "webcam front", &instance).unwrap();
        assert_eq!(cam.name, "front");
        // uuid.uuid5(uuid.UUID("8d3c9b1f6a2e4f0b9c7d5e3a1b2c4d6e"), "moonraker.webcam.front")
        assert_eq!(cam.uid, "215f1988-0b9f-5895-b8af-9b5092652e10");
        assert_eq!(cam.source, "config");
    }

    #[test]
    fn database_record_round_trip() {
        let cam = WebCam::from_request(
            &Args(json!({"name": "cam", "stream_url": "/s", "rotation": "90"}).as_object().unwrap().clone()),
            "u1".into(),
        )
        .unwrap();
        assert_eq!(WebCam::from_database("u1", &cam.db_record()).unwrap(), cam);
        let old = json!({"name": "old", "rotate": 180, "flipX": 1, "enabled": ""});
        let cam = WebCam::from_database("u2", &old).unwrap();
        assert_eq!((cam.rotation, cam.flip_horizontal, cam.enabled), (180, true, false));
        let bad = Args(json!({"name": "x", "stream_url": "/s", "aspect_ratio": "wide"}).as_object().unwrap().clone());
        assert_eq!(
            WebCam::from_request(&bad, "u3".into()).unwrap_err().message,
            "Invalid value for 'aspect_ratio': wide"
        );
    }
}
