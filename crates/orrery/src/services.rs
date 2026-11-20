//! The services that Fluidd lists under "Services", their states, and the units that orrery and
//! Klipper run in. After the `systemd_dbus` provider in Moonraker's `machine.py`; see
//! `docs/design/2026-11-19-service-control.md`.
//!
//! orrery lists the loaded services once at startup and keeps the allowed ones. systemd then
//! reports each change of their state with a `PropertiesChanged` signal, so nothing is polled.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value as Json, json};
use tokio::sync::broadcast::error::RecvError;

use crate::dbus::{Bus, Call, Message, Value};
use crate::server::{App, Paths};

const SYSTEMD: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const UNIT: &str = "org.freedesktop.systemd1.Unit";

/// Moonraker's `default_allowed_services`, written to `moonraker.asvc` when it's missing.
const DEFAULT_ALLOWED: &str = "klipper_mcu
webcamd
MoonCord
KlipperScreen
moonraker-telegram-bot
moonraker-obico
sonar
crowsnest
octoeverywhere
ratos-configurator
";

/// The instance id of a process that isn't in a service. Unit names can't have parentheses, so
/// it can't match a real service, and Fluidd uses it as it is; an empty id would make Fluidd
/// fall back to `klipper` or `moonraker`.
const NOT_A_SERVICE: &str = "(unknown)";

/// Services whose names start with these are always allowed. Moonraker allows `moonraker` and
/// `klipper` ones, such as `klipper-1`; orrery allows its own too.
const ALWAYS_ALLOWED: &[&str] = &["moonraker", "klipper", "orrery"];

pub struct Services {
    /// Whether the provider is `systemd_dbus`.
    enabled: bool,
    allowed: Vec<String>,
    /// The system bus, connected on first use. `None` when it isn't there.
    bus: tokio::sync::OnceCell<Option<Bus>>,
    list: Mutex<Vec<Service>>,
    /// The services orrery and Klipper run in, such as `orrery` for `orrery.service`.
    own_unit: Mutex<Option<String>>,
    klipper_unit: Mutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Service {
    name: String,
    /// The unit's D-Bus object, which its signals come from.
    path: String,
    active_state: String,
    sub_state: String,
}

impl Services {
    pub fn new(provider: &str, paths: &Paths) -> Self {
        Self {
            enabled: provider == "systemd_dbus",
            allowed: read_allowed(&paths.data.join("moonraker.asvc")),
            bus: tokio::sync::OnceCell::new(),
            list: Mutex::new(Vec::new()),
            own_unit: Mutex::new(None),
            klipper_unit: Mutex::new(None),
        }
    }

    pub fn is_allowed(&self, name: &str) -> bool {
        self.allowed.iter().any(|s| s == name) || ALWAYS_ALLOWED.iter().any(|p| name.starts_with(p))
    }

    async fn bus(&self) -> Option<&Bus> {
        self.bus
            .get_or_init(|| async {
                if !self.enabled {
                    return None;
                }
                match Bus::system().await {
                    Ok(bus) => Some(bus),
                    Err(e) => {
                        tracing::warn!("orrery can't connect to the system bus: {e}");
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    /// `available_services` and `service_state`, as `machine.system_info` has them.
    pub fn system_info(&self) -> (Json, Json) {
        let list = self.list.lock().expect("services lock");
        let names = list.iter().map(|s| json!(s.name)).collect();
        let states: Map<String, Json> = list.iter().map(|s| (s.name.clone(), s.state())).collect();
        (Json::Array(names), Json::Object(states))
    }

    /// `instance_ids`: the services orrery and Klipper run in. When one isn't in a service,
    /// such as when it was started from a terminal, its id is [`NOT_A_SERVICE`]. A default
    /// like `klipper` could name another printer's Klipper on a machine with several, and
    /// Fluidd's restart button would restart that one. Without a provider nothing can be
    /// controlled, so the ids are Moonraker's defaults.
    pub fn instance_ids(&self) -> Json {
        if !self.enabled {
            return json!({"moonraker": "moonraker", "klipper": "klipper"});
        }
        let own = self.own_unit.lock().expect("unit lock").clone();
        let klipper = self.klipper_unit.lock().expect("unit lock").clone();
        json!({
            "moonraker": own.as_deref().unwrap_or(NOT_A_SERVICE),
            "klipper": klipper.as_deref().unwrap_or(NOT_A_SERVICE),
        })
    }

    /// The service a process runs in, such as `klipper` for `klipper.service`. A process
    /// started from a terminal is in a session scope, not a service, and gets `None`.
    async fn unit_of(&self, pid: u32) -> Option<String> {
        let bus = self.bus().await?;
        let reply = bus.call(manager("GetUnitByPID", vec![Value::U32(pid)])).await.ok()?;
        let path = reply.first()?.as_str()?;
        let id = bus.property(SYSTEMD, path, UNIT, "Id").await.ok()?;
        id.as_str()?.strip_suffix(".service").map(str::to_owned)
    }

    /// Records the service Klippy runs in, from the process id of its socket's other end. A
    /// Klippy that isn't in a service clears the one found before, which may be another's.
    pub async fn found_klippy(&self, pid: Option<u32>) {
        if !self.enabled {
            return;
        }
        let unit = match pid {
            Some(pid) => self.unit_of(pid).await,
            None => None,
        };
        let mut known = self.klipper_unit.lock().expect("unit lock");
        if *known != unit {
            match &unit {
                Some(unit) => tracing::info!("Klipper runs in {unit}.service"),
                None => tracing::info!("Klipper doesn't run in a service, so orrery can't restart it"),
            }
            *known = unit;
        }
    }

    /// Applies new states to the service whose object is `path`, and returns it when it
    /// changed.
    fn update(&self, path: &str, active: Option<&str>, sub: Option<&str>) -> Option<Service> {
        let mut list = self.list.lock().expect("services lock");
        let svc = list.iter_mut().find(|s| s.path == path)?;
        let before = svc.clone();
        if let Some(a) = active {
            svc.active_state = a.to_owned();
        }
        if let Some(s) = sub {
            svc.sub_state = s.to_owned();
        }
        (*svc != before).then(|| svc.clone())
    }
}

impl Service {
    fn state(&self) -> Json {
        json!({"active_state": self.active_state, "sub_state": self.sub_state})
    }
}

fn manager(member: &str, args: Vec<Value>) -> Call<'_> {
    Call { destination: SYSTEMD, path: MANAGER_PATH, interface: MANAGER, member, args }
}

/// Reads `moonraker.asvc`, one service a line, and writes Moonraker's list when it's missing.
fn read_allowed(path: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Err(e) = std::fs::write(path, DEFAULT_ALLOWED) {
                tracing::warn!("orrery can't write {}: {e}", path.display());
            }
            DEFAULT_ALLOWED.to_owned()
        }
        Err(e) => {
            tracing::warn!("orrery can't read {}, so it allows the default services: {e}", path.display());
            DEFAULT_ALLOWED.to_owned()
        }
    };
    let mut allowed: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let name = line.strip_suffix(".service").unwrap_or(line);
        if !allowed.iter().any(|a| a == name) {
            allowed.push(name.to_owned());
        }
    }
    allowed
}

/// The allowed services in a `ListUnitsFiltered` reply. Its units are structs of name,
/// description, load state, active state, sub state, following, and object path, then job
/// fields. Moonraker takes every unit type and cuts each name at its first dot; orrery takes
/// only services, so `klipper.socket` can't stand in for `klipper.service`.
fn parse_units(reply: &[Value], allowed: impl Fn(&str) -> bool) -> Vec<Service> {
    let mut list: Vec<Service> = Vec::new();
    for unit in reply.first().and_then(Value::items).unwrap_or_default() {
        let f = unit.items().unwrap_or_default();
        let text = |i: usize| f.get(i).and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(name) = text(0).strip_suffix(".service").map(str::to_owned) else { continue };
        if !allowed(&name) || list.iter().any(|s| s.name == name) {
            continue;
        }
        list.push(Service { name, path: text(6), active_state: text(3), sub_state: text(4) });
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

/// Connects to systemd, lists the services, and follows their states until the bus closes.
pub async fn run(app: Arc<App>) {
    let services = &app.machine.services;
    if !services.enabled {
        return;
    }
    let Some(bus) = services.bus().await else {
        app.add_warning("[machine]: D-Bus isn't available, so orrery can't list or control services.");
        return;
    };
    if let Some(unit) = services.unit_of(std::process::id()).await {
        tracing::info!("orrery runs in {unit}.service");
        *services.own_unit.lock().expect("unit lock") = Some(unit);
    }
    // systemd sends units' signals only while a client has subscribed. Others usually have,
    // but orrery doesn't count on it.
    if let Err(e) = bus.call(manager("Subscribe", vec![])).await {
        tracing::info!("systemd refused orrery's Subscribe call: {e}");
    }
    // Subscribed before the list is read, so no change falls in between.
    let rule = format!(
        "type='signal',sender='{SYSTEMD}',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',arg0='{UNIT}'"
    );
    let mut signals = match bus.subscribe(&rule).await {
        Ok(rx) => rx,
        Err(e) => {
            app.add_warning(format!("[machine]: orrery can't follow the services' states: {e}"));
            return;
        }
    };
    match bus.call(manager("ListUnitsFiltered", vec![Value::strs(&["loaded"])])).await {
        Ok(reply) => {
            let list = parse_units(&reply, |n| services.is_allowed(n));
            let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
            tracing::info!("Services: {}", names.join(", "));
            *services.list.lock().expect("services lock") = list;
        }
        Err(e) => {
            app.add_warning(format!("[machine]: orrery can't list the services: {e}"));
            return;
        }
    }
    loop {
        match signals.recv().await {
            Ok(msg) => changed(&app, &msg),
            // Signals were missed, so read every state again.
            Err(RecvError::Lagged(_)) => refresh(&app, bus).await,
            Err(RecvError::Closed) => {
                tracing::warn!("Service states stop updating: the D-Bus connection closed");
                return;
            }
        }
    }
}

/// A unit's `PropertiesChanged` signal: the interface, the changed properties, and the
/// invalidated ones.
fn changed(app: &App, msg: &Message) {
    if msg.member.as_deref() != Some("PropertiesChanged") || msg.body.first().and_then(Value::as_str) != Some(UNIT) {
        return;
    }
    let (Some(path), Some(props)) = (msg.path.as_deref(), msg.body.get(1)) else { return };
    let active = props.get("ActiveState").and_then(Value::as_str);
    let sub = props.get("SubState").and_then(Value::as_str);
    if active.is_none() && sub.is_none() {
        return;
    }
    if let Some(svc) = app.machine.services.update(path, active, sub) {
        notify(app, &svc);
    }
}

async fn refresh(app: &App, bus: &Bus) {
    let paths: Vec<String> =
        app.machine.services.list.lock().expect("services lock").iter().map(|s| s.path.clone()).collect();
    for path in paths {
        let active = bus.property(SYSTEMD, &path, UNIT, "ActiveState").await.ok();
        let sub = bus.property(SYSTEMD, &path, UNIT, "SubState").await.ok();
        let (active, sub) = (active.as_ref().and_then(Value::as_str), sub.as_ref().and_then(Value::as_str));
        if let Some(svc) = app.machine.services.update(&path, active, sub) {
            notify(app, &svc);
        }
    }
}

fn notify(app: &App, svc: &Service) {
    tracing::info!("Service {} is {} ({})", svc.name, svc.active_state, svc.sub_state);
    app.notify("service_state_changed", vec![json!({ &svc.name: svc.state() })]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, active: &str, sub: &str) -> Value {
        let path = format!("/org/freedesktop/systemd1/unit/{}", name.replace('.', "_2e"));
        Value::Struct(vec![
            Value::str(name),
            Value::str("description"),
            Value::str("loaded"),
            Value::str(active),
            Value::str(sub),
            Value::str(""),
            Value::Path(path),
            Value::U32(0),
            Value::str(""),
            Value::path("/"),
        ])
    }

    #[test]
    fn keeps_allowed_services() {
        let dir = tempfile::tempdir().unwrap();
        let asvc = dir.path().join("moonraker.asvc");
        std::fs::write(&asvc, "crowsnest\n  aldis.service \n\ncrowsnest\n").unwrap();
        let allowed = read_allowed(&asvc);
        assert_eq!(allowed, ["crowsnest", "aldis"]);
        let reply = [Value::Array(
            "(ssssssouso)".into(),
            vec![
                unit("ssh.service", "active", "running"),
                unit("klipper.socket", "active", "listening"),
                unit("klipper.service", "active", "running"),
                unit("klipper-mcu.service", "active", "running"),
                unit("orrery-test.service", "active", "running"),
                unit("crowsnest.service", "inactive", "dead"),
                unit("aldis.service", "failed", "failed"),
            ],
        )];
        let is_allowed = |n: &str| allowed.iter().any(|a| a == n) || ALWAYS_ALLOWED.iter().any(|p| n.starts_with(p));
        let list = parse_units(&reply, is_allowed);
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["aldis", "crowsnest", "klipper", "klipper-mcu", "orrery-test"]);
        assert_eq!(list[2].path, "/org/freedesktop/systemd1/unit/klipper_2eservice");
        assert_eq!((list[1].active_state.as_str(), list[1].sub_state.as_str()), ("inactive", "dead"));
    }

    #[test]
    fn writes_the_default_list() {
        let dir = tempfile::tempdir().unwrap();
        let asvc = dir.path().join("moonraker.asvc");
        assert!(read_allowed(&asvc).contains(&"crowsnest".to_owned()));
        assert_eq!(std::fs::read_to_string(&asvc).unwrap(), DEFAULT_ALLOWED);
    }
}
