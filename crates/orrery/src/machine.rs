//! `machine.system_info`, after the system info parts of Moonraker's `machine.py`: the CPU, the
//! SD card, the distribution, virtualization, and network interfaces. The services, and
//! starting and stopping them, are in [`crate::services`].

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::api::{ApiResult, RequestType, Transports, handler};
use crate::config::Config;
use crate::server::{App, Paths};
use crate::services::Services;

const PROVIDERS: &[&str] = &["none", "systemd_cli", "systemd_dbus", "supervisord_cli"];
const SD_CID_PATH: &str = "/sys/block/mmcblk0/device/cid";
const SD_CSD_PATH: &str = "/sys/block/mmcblk0/device/csd";
/// Network interfaces are read on every tenth process stats update.
pub const NETWORK_UPDATE_SEQUENCE: u64 = 10;
/// The `systemd` virtualization ids that are containers.
const CONTAINER_TYPES: &[&str] =
    &["openvz", "lxc", "lxc-libvirt", "systemd-nspawn", "docker", "podman", "rkt", "wsl", "proot", "pouch"];

pub struct Machine {
    provider: String,
    inside_container: bool,
    system_info: Mutex<Map<String, Value>>,
    pub services: Services,
}

impl Machine {
    pub fn new(config: &Config, paths: &Paths) -> Result<Self> {
        let provider = config.section("machine").get_str("provider", "systemd_dbus");
        if !PROVIDERS.contains(&provider.as_str()) {
            bail!("[machine] provider: Invalid Provider: {provider}");
        }
        let (virtualization, inside_container) = check_inside_container();
        let mut info = Map::new();
        info.insert("cpu_info".into(), cpu_info());
        info.insert("sd_info".into(), sd_info());
        info.insert("distribution".into(), distribution());
        info.insert("virtualization".into(), virtualization);
        info.insert("network".into(), json!({}));
        info.insert("canbus".into(), json!({}));
        info.insert("provider".into(), json!(provider));
        tracing::info!("Using System Provider: {provider}");
        let shutdown_action = config.section("machine").get_str("shutdown_action", "poweroff").to_lowercase();
        if !["halt", "poweroff"].contains(&shutdown_action.as_str()) {
            bail!("[machine] shutdown_action: {shutdown_action} isn't a valid choice. It must be halt or poweroff.");
        }
        let services = Services::new(&provider, shutdown_action, paths);
        Ok(Self { provider, inside_container, system_info: Mutex::new(info), services })
    }

    /// The kind of container orrery runs in, such as `docker`, or `None` outside one.
    pub fn container(&self) -> Option<String> {
        if !self.inside_container {
            return None;
        }
        let info = self.info();
        Some(info["virtualization"]["virt_identifier"].as_str().unwrap_or("unknown").to_owned())
    }

    fn info(&self) -> std::sync::MutexGuard<'_, Map<String, Value>> {
        self.system_info.lock().expect("system info lock")
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register("/machine/system_info", &[RequestType::Get], Transports::All, handler(|app, _| system_info(app)));
    drop(r);
    crate::services::register_endpoints(app);
}

pub fn start(app: &Arc<App>) {
    tokio::spawn(crate::services::run(app.clone()));
    tokio::spawn(crate::services::start_agent_units(app.clone()));
    let app = app.clone();
    tokio::spawn(async move {
        if !app.machine.inside_container {
            let virt = check_virt_status(&app.machine.provider).await;
            app.machine.info().insert("virtualization".into(), virt);
        }
        update_network(&app).await;
    });
}

async fn system_info(app: Arc<App>) -> ApiResult {
    let mut info = app.machine.info().clone();
    let (names, states) = app.machine.services.system_info();
    info.insert("available_services".into(), names);
    info.insert("service_state".into(), states);
    info.insert("instance_ids".into(), app.machine.services.instance_ids());
    Ok(json!({"system_info": info}))
}

/// Moonraker's `_get_cpu_info`.
fn cpu_info() -> Value {
    let uname = rustix::system::uname();
    let mut info = json!({
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "bits": if cfg!(target_pointer_width = "64") { "64bit" } else { "32bit" },
        "processor": uname.machine().to_string_lossy(),
        "cpu_desc": "",
        "serial_number": "",
        "hardware_desc": "",
        "model": "",
        "total_memory": null,
        "memory_units": "",
    });
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        let items: Vec<&str> = text.trim().split("\n\n").map(str::trim).filter(|i| !i.is_empty()).collect();
        let desc = Regex::new(r"model name\s+:\s+(.+)").expect("valid regex");
        if let Some(c) = items.iter().find_map(|i| desc.captures(i)) {
            info["cpu_desc"] = json!(c[1].trim());
        }
        if let Some(last) = items.last() {
            for (key, pattern) in [
                ("hardware_desc", r"Hardware\s+:\s+(.+)"),
                ("serial_number", r"Serial\s+:\s+0*(.+)"),
                ("model", r"Model\s+:\s+(.+)"),
            ] {
                if let Some(c) = Regex::new(pattern).expect("valid regex").captures(last) {
                    info[key] = json!(c[1].trim());
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo")
        && let Some(line) = text.lines().map(str::trim).find(|l| l.starts_with("MemTotal:"))
    {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let (Some(total), Some(units)) = (parts.get(1).and_then(|t| t.parse::<u64>().ok()), parts.get(2)) {
            info["total_memory"] = json!(total);
            info["memory_units"] = json!(units);
        }
    }
    info
}

/// Moonraker's `_get_sdcard_info`: decodes the SD card's CID and CSD registers.
fn sd_info() -> Value {
    let Ok(cid) = std::fs::read_to_string(SD_CID_PATH) else { return json!({}) };
    match parse_sd(&cid, std::fs::read_to_string(SD_CSD_PATH).ok().as_deref()) {
        Some(info) => Value::Object(info),
        None => {
            tracing::info!("Error reading SDCard CID Register");
            json!({})
        }
    }
}

fn parse_sd(cid: &str, csd: Option<&str>) -> Option<Map<String, Value>> {
    let cid = cid.trim().to_lowercase();
    let hex = |r: std::ops::Range<usize>| u32::from_str_radix(cid.get(r)?, 16).ok();
    let mid = cid.get(..2)?;
    let manufacturer = match mid {
        "1b" => "Samsung",
        "03" => "Sandisk",
        "74" => "PNY",
        _ => "Unknown",
    };
    let product: String = decode_hex(cid.get(6..16)?)?.into_iter().filter(u8::is_ascii).map(char::from).collect();
    let mut info = Map::new();
    info.insert("manufacturer_id".into(), json!(mid));
    info.insert("manufacturer".into(), json!(manufacturer));
    info.insert("oem_id".into(), json!(cid.get(2..6)?));
    info.insert("product_name".into(), json!(product));
    info.insert("product_revision".into(), json!(format!("{}.{}", hex(16..17)?, hex(17..18)?)));
    info.insert("serial_number".into(), json!(cid.get(18..26)?));
    info.insert("manufacturer_date".into(), json!(format!("{}/{}", hex(29..30)?, hex(27..29)? + 2000)));
    info.insert("capacity".into(), json!("Unknown"));
    info.insert("total_bytes".into(), json!(0));
    let Some(reg) = csd.and_then(|c| decode_hex(c.trim())) else {
        tracing::info!("Error Reading SDCard CSD Register");
        return Some(info);
    };
    let b = |i: usize| u64::from(reg.get(i).copied().unwrap_or_default());
    let (total, capacity) = match (b(0) >> 6) & 0x3 {
        0 => {
            let block_len = 1u64 << (b(5) & 0xF);
            let c_size = ((b(6) & 0x3) << 10) | (b(7) << 2) | ((b(8) >> 6) & 0x3);
            let c_mult = 1u64 << ((((b(9) & 0x3) << 1) | (b(10) >> 7)) + 2);
            let total = (c_size + 1) * c_mult * block_len;
            (total, format!("{:.1} MiB", total as f64 / 1024f64.powi(2)))
        }
        1 => {
            let total = ((((b(7) & 0x3F) << 16) | (b(8) << 8) | b(9)) + 1) * 512 * 1024;
            (total, format!("{:.1} GiB", total as f64 / 1024f64.powi(3)))
        }
        2 => {
            let total = ((((b(6) & 0xF) << 24) | (b(7) << 16) | (b(8) << 8) | b(9)) + 1) * 512 * 1024;
            (total, format!("{:.1} TiB", total as f64 / 1024f64.powi(4)))
        }
        _ => return Some(info),
    };
    info.insert("capacity".into(), json!(capacity));
    info.insert("total_bytes".into(), json!(total));
    Some(info)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// The `distribution` fields Python's `distro` package gives Moonraker, read from
/// `/etc/os-release`, with `/etc/lsb-release` as the fallback.
fn distribution() -> Value {
    let os = read_key_values(Path::new("/etc/os-release"))
        .or_else(|| read_key_values(Path::new("/usr/lib/os-release")))
        .unwrap_or_default();
    let lsb = read_key_values(Path::new("/etc/lsb-release")).unwrap_or_default();
    distribution_from(&os, &lsb, &release_info(), &rustix::system::uname().release().to_string_lossy())
}

fn distribution_from(
    os: &Map<String, Value>,
    lsb: &Map<String, Value>,
    release: &Map<String, Value>,
    kernel: &str,
) -> Value {
    let get = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let first = |values: [String; 3]| values.into_iter().find(|v| !v.is_empty()).unwrap_or_default();
    let mut id = first([get(os, "ID"), get(lsb, "DISTRIB_ID"), get(release, "id")]).to_lowercase().replace(' ', "_");
    if let Some(normal) = match id.as_str() {
        "ol" => Some("oracle"),
        "opensuse-leap" => Some("opensuse"),
        "enterpriseenterpriseas" | "enterpriseenterpriseserver" => Some("oracle"),
        "redhat" => Some("rhel"),
        "redhatenterpriseworkstation" | "redhatenterpriseserver" | "redhatenterprisecomputenode" => Some("rhel"),
        _ => None,
    } {
        id = normal.to_owned();
    }
    let name = first([get(os, "PRETTY_NAME"), get(lsb, "DISTRIB_DESCRIPTION"), get(release, "name")]);
    let version = first([get(os, "VERSION_ID"), get(lsb, "DISTRIB_RELEASE"), get(release, "version_id")]);
    let parts = Regex::new(r"(\d+)\.?(\d+)?\.?(\d+)?").expect("valid regex");
    let part = |i: usize| parts.captures(&version).and_then(|c| c.get(i)).map(|m| m.as_str()).unwrap_or_default();
    let mut codename = get(os, "VERSION_CODENAME");
    if codename.is_empty() {
        codename = get(os, "UBUNTU_CODENAME");
    }
    if codename.is_empty()
        && let Some(c) = Regex::new(r"\((\D+)\)|,\s*(\D+)$").expect("valid regex").captures(&get(os, "VERSION"))
    {
        codename = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().trim().to_owned()).unwrap_or_default();
    }
    if codename.is_empty() {
        codename = first([get(lsb, "DISTRIB_CODENAME"), get(release, "codename"), String::new()]);
    }
    json!({
        "name": name,
        "id": id,
        "version": version,
        "version_parts": {"major": part(1), "minor": part(2), "build_number": part(3)},
        "like": get(os, "ID_LIKE"),
        "codename": codename,
        "release_info": release,
        "kernel_version": kernel,
    })
}

/// `KEY=value` lines, with shell-style quotes removed.
fn read_key_values(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter_map(|l| l.trim().split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), json!(v.trim().trim_matches(['"', '\'']))))
            .collect(),
    )
}

/// `distro.distro_release_info()`: the first `/etc/<name>-release` file whose first line reads
/// like "Name release 1.2 (codename)".
fn release_info() -> Map<String, Value> {
    const IGNORED: &[&str] = &[
        "debian_version",
        "lsb-release",
        "oem-release",
        "os-release",
        "system-release",
        "plesk-release",
        "iredmail-release",
        "board-release",
        "ec2_version",
    ];
    let Ok(dir) = std::fs::read_dir("/etc") else { return Map::new() };
    let file_name = Regex::new(r"^(\w+)[-_](release|version)$").expect("valid regex");
    let line = Regex::new(r"^(?:Linux\s+)?(.+?)\s+release\s+([\d.]+)\s*(?:\((.+)\))?").expect("valid regex");
    let mut names: Vec<String> = dir.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    names.sort();
    for name in names {
        let Some(m) = file_name.captures(&name) else { continue };
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(Path::new("/etc").join(&name)) else { continue };
        let Some(c) = line.captures(text.lines().next().unwrap_or_default()) else { continue };
        let mut info = Map::new();
        info.insert("name".into(), json!(c[1]));
        info.insert("version_id".into(), json!(c[2]));
        if let Some(code) = c.get(3) {
            info.insert("codename".into(), json!(code.as_str()));
        }
        info.insert("id".into(), json!(m[1]));
        return info;
    }
    Map::new()
}

/// Moonraker's `_check_inside_container`. Returns the virtualization info and whether orrery
/// runs in a container.
fn check_inside_container() -> (Value, bool) {
    let found = |virt_id: &str| (json!({"virt_type": "container", "virt_identifier": virt_id}), true);
    if let Ok(data) = std::fs::read_to_string("/proc/1/cgroup") {
        for ct in ["docker", "lxc"] {
            if data.contains(ct) {
                tracing::info!("Container detected via cgroup: {ct}");
                return found(ct);
            }
        }
    }
    if let Ok(data) = std::fs::read_to_string("/proc/1/sched")
        && let Some(proc_name) = data.split_whitespace().next()
        && !matches!(proc_name, "init" | "systemd")
    {
        let id = if Path::new("/.dockerenv").exists() || Path::new("/.dockerinit").exists() { "docker" } else { "lxc" };
        tracing::info!("Container detected via sched: {id}");
        return found(id);
    }
    (json!({"virt_type": "none", "virt_identifier": "none"}), false)
}

/// The provider's `check_virt_status`. orrery asks `systemd-detect-virt` for every systemd
/// provider; Moonraker's default provider asks systemd over D-Bus.
async fn check_virt_status(provider: &str) -> Value {
    if !provider.starts_with("systemd") {
        return json!({"virt_type": "unknown", "virt_identifier": "unknown"});
    }
    let output = tokio::process::Command::new("systemd-detect-virt").output().await;
    let virt_id = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => "none".to_owned(),
    };
    if virt_id == "none" || virt_id.is_empty() {
        tracing::info!("No Virtualization Detected");
        return json!({"virt_type": "none", "virt_identifier": "none"});
    }
    let virt_type = if CONTAINER_TYPES.contains(&virt_id.as_str()) { "container" } else { "vm" };
    tracing::info!("Virtualized Environment Detected, Type: {virt_type} id: {virt_id}");
    json!({"virt_type": virt_type, "virt_identifier": virt_id})
}

/// The address this host uses to reach other networks, as Moonraker's `_find_public_ip`: the
/// source address of a UDP socket "connected" to a broadcast or remote address. Nothing is sent.
pub fn public_ip() -> Option<std::net::IpAddr> {
    use std::net::UdpSocket;
    let attempts: [(&str, &str, bool); 3] = [
        ("0.0.0.0:0", "255.255.255.255:1", true),
        ("0.0.0.0:0", "10.255.255.255:1", false),
        ("[::]:0", "[2001:db8::1234]:1", false),
    ];
    attempts.into_iter().find_map(|(bind, target, broadcast)| {
        let socket = UdpSocket::bind(bind).ok()?;
        if broadcast {
            socket.set_broadcast(true).ok()?;
        }
        socket.connect(target).ok()?;
        Some(socket.local_addr().ok()?.ip())
    })
}

/// Every address of this host's network interfaces.
pub fn local_ips(app: &App) -> Vec<String> {
    let info = app.machine.info();
    info["network"]
        .as_object()
        .into_iter()
        .flat_map(|n| n.values())
        .flat_map(|i| i["ip_addresses"].as_array().cloned().unwrap_or_default())
        .filter_map(|a| a["address"].as_str().map(str::to_owned))
        .collect()
}

/// Moonraker's `_parse_network_interfaces`: reads `ip -json -det address`.
pub async fn update_network(app: &Arc<App>) {
    let output = tokio::process::Command::new("ip").args(["-json", "-det", "address"]).output().await;
    let interfaces: Vec<Value> = match output {
        Ok(out) if out.status.success() => match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                tracing::info!("Error processing network update: {e}");
                return;
            }
        },
        Ok(out) => {
            tracing::info!("Error processing network update: {}", String::from_utf8_lossy(&out.stderr).trim());
            return;
        }
        Err(e) => {
            tracing::info!("Error processing network update: {e}");
            return;
        }
    };
    let (network, canbus) = parse_interfaces(&interfaces);
    let mut info = app.machine.info();
    info.insert("network".into(), Value::Object(network));
    info.insert("canbus".into(), Value::Object(canbus));
}

fn parse_interfaces(interfaces: &[Value]) -> (Map<String, Value>, Map<String, Value>) {
    let mut network = Map::new();
    let mut canbus = Map::new();
    for intf in interfaces {
        if intf["operstate"] != "UP" {
            continue;
        }
        let Some(name) = intf["ifname"].as_str() else { continue };
        if intf["link_type"] == "can" {
            let data = &intf["linkinfo"]["info_data"];
            canbus.insert(
                name.to_owned(),
                json!({
                    "tx_queue_len": intf["txqlen"],
                    "bitrate": data["bittiming"].get("bitrate").cloned().unwrap_or(json!(-1)),
                    "driver": data["bittiming_const"].get("name").cloned().unwrap_or(json!("unknown")),
                }),
            );
        } else if intf["link_type"] == "ether"
            && let Some(mac) = intf.get("address")
        {
            let addresses: Vec<Value> = intf["addr_info"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    let family = match a.get("family")?.as_str()? {
                        "inet" => "ipv4",
                        "inet6" => "ipv6",
                        _ => return None,
                    };
                    Some(json!({"family": family, "address": a.get("local")?, "is_link_local": a["scope"] == "link"}))
                })
                .collect();
            if !addresses.is_empty() {
                network.insert(name.to_owned(), json!({"mac_address": mac, "ip_addresses": addresses}));
            }
        }
    }
    (network, canbus)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn distribution_like_distro() {
        let os = map(json!({"PRETTY_NAME": "Debian GNU/Linux 12 (bookworm)", "ID": "debian", "VERSION_ID": "12",
            "VERSION": "12 (bookworm)", "VERSION_CODENAME": "bookworm"}));
        let d = distribution_from(&os, &Map::new(), &Map::new(), "6.1.0");
        assert_eq!(d["name"], "Debian GNU/Linux 12 (bookworm)");
        assert_eq!(d["version_parts"], json!({"major": "12", "minor": "", "build_number": ""}));
        assert_eq!(d["codename"], "bookworm");
        assert_eq!(d["like"], "");
        let os = map(json!({"ID": "ubuntu", "VERSION_ID": "26.04", "VERSION": "26.04.1 LTS (Resolute Raccoon)"}));
        assert_eq!(distribution_from(&os, &Map::new(), &Map::new(), "")["codename"], "Resolute Raccoon");
    }

    #[test]
    fn sd_card_registers() {
        // A 32 GB SanDisk card.
        let info = parse_sd("035344534333324780e1a7c6b8012a00", Some("400e00325b590000ed2f7f800a404000")).unwrap();
        assert_eq!(info["manufacturer"], "Sandisk");
        assert_eq!(info["product_name"], "SC32G");
        assert_eq!(info["product_revision"], "8.0");
        assert_eq!(info["total_bytes"], 31834767360u64);
        assert_eq!(info["manufacturer_date"], "10/2018");
        assert_eq!(info["capacity"], "29.6 GiB");
    }

    #[test]
    fn network_interfaces() {
        let ip = json!([
            {"ifname": "lo", "operstate": "UNKNOWN", "link_type": "loopback"},
            {"ifname": "eth0", "operstate": "UP", "link_type": "ether", "address": "aa:bb:cc:dd:ee:ff",
             "addr_info": [{"family": "inet", "local": "10.0.0.5", "scope": "global"},
                           {"family": "inet6", "local": "fe80::1", "scope": "link"}]},
            {"ifname": "can0", "operstate": "UP", "link_type": "can", "txqlen": 1024,
             "linkinfo": {"info_data": {"bittiming": {"bitrate": 1000000}, "bittiming_const": {"name": "gs_usb"}}}}
        ]);
        let (net, can) = parse_interfaces(ip.as_array().unwrap());
        assert_eq!(
            net["eth0"]["ip_addresses"][1],
            json!({"family": "ipv6", "address": "fe80::1", "is_link_local": true})
        );
        assert_eq!(can["can0"], json!({"tx_queue_len": 1024, "bitrate": 1000000, "driver": "gs_usb"}));
        assert!(!net.contains_key("lo"));
    }
}
