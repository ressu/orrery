//! `machine.proc_stats` and `notify_proc_stat_update`, after Moonraker's `proc_stats.py`.
//!
//! Once a second orrery measures its own CPU use and memory, the CPU temperature, network
//! traffic, and the system's CPU and memory use. On a Raspberry Pi it also checks the
//! firmware's throttle flags every ten seconds and sends `notify_cpu_throttled` when they change.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;

use crate::api::{ApiResult, RequestType, Transports, handler};
use crate::server::{App, Event};

const VC_GEN_CMD_FILE: &str = "/usr/bin/vcgencmd";
const VCIO_PATH: &str = "/dev/vcio";
const TEMPERATURE_PATH: &str = "/sys/class/thermal/thermal_zone0/temp";
const HWMON_ROOT_PATH: &str = "/sys/class/hwmon/";
const HWMON_PLATFORMS: &[(&str, &str)] = &[("coretemp", "Intel"), ("k10temp", "AMD"), ("cpu_thermal", "Raspberry Pi")];
const STAT_UPDATE_TIME: Duration = Duration::from_secs(1);
const REPORT_QUEUE_SIZE: usize = 30;
const THROTTLE_CHECK_INTERVAL: u64 = 10;
const THROTTLED_FLAGS: &[(u32, &str)] = &[
    (1, "Under-Voltage Detected"),
    (1 << 1, "Frequency Capped"),
    (1 << 2, "Currently Throttled"),
    (1 << 3, "Temperature Limit Active"),
    (1 << 16, "Previously Under-Volted"),
    (1 << 17, "Previously Frequency Capped"),
    (1 << 18, "Previously Throttled"),
    (1 << 19, "Previously Temperature Limited"),
];

#[derive(Default)]
struct Stats {
    queue: VecDeque<Value>,
    last_net: Map<String, Value>,
    last_cpu: HashMap<String, (u64, u64)>,
    cpu_usage: Map<String, Value>,
    memory_usage: Map<String, Value>,
    total_throttled: u32,
    last_throttled: u32,
}

pub struct ProcStats {
    temp_file: PathBuf,
    vcgencmd: bool,
    stats: Mutex<Stats>,
    /// The throttle check talks to the firmware one request at a time.
    throttle_lock: tokio::sync::Mutex<()>,
}

impl Default for ProcStats {
    fn default() -> Self {
        let vcgencmd = Path::new(VC_GEN_CMD_FILE).exists() && Path::new(VCIO_PATH).exists();
        if vcgencmd {
            tracing::info!("Detected 'vcgencmd', throttle checking enabled");
        } else {
            tracing::info!("Unable to find 'vcgencmd', throttle checking disabled");
        }
        Self {
            temp_file: cpu_thermal_file(),
            vcgencmd,
            stats: Mutex::new(Stats::default()),
            throttle_lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl ProcStats {
    fn stats(&self) -> std::sync::MutexGuard<'_, Stats> {
        self.stats.lock().expect("proc stats lock")
    }

    fn cpu_temperature(&self) -> Option<f64> {
        let raw: i64 = std::fs::read_to_string(&self.temp_file).ok()?.trim().parse().ok()?;
        Some(raw as f64 / 1000.0)
    }

    /// Moonraker's `_check_throttled_state`, or `None` when the system has no `vcgencmd`.
    async fn throttled_state(&self) -> Option<Value> {
        if !self.vcgencmd {
            return None;
        }
        let _guard = self.throttle_lock.lock().await;
        let resp = tokio::task::spawn_blocking(vcgencmd::get_throttled).await.ok().flatten();
        let bits =
            resp.and_then(|r| u32::from_str_radix(r.trim().rsplit('=').next()?.trim_start_matches("0x"), 16).ok());
        Some(match bits {
            Some(bits) => {
                let flags: Vec<&str> =
                    THROTTLED_FLAGS.iter().filter(|(flag, _)| flag & bits != 0).map(|(_, d)| *d).collect();
                json!({"bits": bits, "flags": flags})
            }
            None => json!({"bits": 0, "flags": ["?"]}),
        })
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register("/machine/proc_stats", &[RequestType::Get], Transports::All, handler(|app, _| stat_request(app)));
}

pub fn start(app: &Arc<App>) {
    tokio::spawn(update_loop(app.clone()));
    tokio::spawn(follow_events(app.clone(), app.events.subscribe()));
}

async fn stat_request(app: Arc<App>) -> ApiResult {
    let ps = &app.proc_stats;
    let throttled = ps.throttled_state().await;
    let cpu_temp = ps.cpu_temperature();
    let st = ps.stats();
    Ok(json!({
        "moonraker_stats": Vec::from(st.queue.clone()),
        "throttled_state": throttled,
        "cpu_temp": cpu_temp,
        "network": st.last_net,
        "system_cpu_usage": st.cpu_usage,
        "system_uptime": seconds(rustix::time::clock_gettime(rustix::time::ClockId::Boottime)),
        "system_memory": st.memory_usage,
        "websocket_connections": app.client_count(),
    }))
}

/// Logs the recent stats when Klippy shuts down, as Moonraker does.
async fn follow_events(app: Arc<App>, mut rx: broadcast::Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(Event::KlippyShutdown) => {
                let ps = &app.proc_stats;
                let mut msg = "\nMoonraker System Usage Statistics:".to_owned();
                for stats in &ps.stats().queue {
                    msg.push('\n');
                    msg.push_str(&format_stats(stats));
                }
                msg.push_str(&format!("\nCPU Temperature: {}", python_option(ps.cpu_temperature())));
                tracing::info!("{msg}");
                if let Some(ts) = ps.throttled_state().await {
                    let flags: Vec<&str> =
                        ts["flags"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    tracing::info!("Throttled Flags: {}", flags.join(" "));
                }
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn format_stats(stats: &Value) -> String {
    format!(
        "System Time: {:.6}, Usage: {}%, Memory: {} {}",
        stats["time"].as_f64().unwrap_or_default(),
        stats["cpu_usage"],
        python_option(stats["memory"].as_u64()),
        stats["mem_units"].as_str().unwrap_or("None"),
    )
}

fn python_option<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "None".to_owned(), |v| v.to_string())
}

/// Moonraker's `_handle_stat_update`, once a second.
async fn update_loop(app: Arc<App>) {
    let ps = &app.proc_stats;
    let mut last_update = Instant::now();
    let mut last_proc = process_time();
    let mut sequence: u64 = 0;
    loop {
        let now = Instant::now();
        let proc_time = process_time();
        let time_diff = now.duration_since(last_update).as_secs_f64().max(f64::EPSILON);
        let usage = round2((proc_time - last_proc) / time_diff * 100.0);
        let (memory, units) = memory_usage();
        let cpu_temp = ps.cpu_temperature();
        let mut net = net_stats();
        let notification = {
            let mut st = ps.stats();
            for (dev, stats) in net.iter_mut() {
                let total = |s: &Value| s["rx_bytes"].as_u64().unwrap_or(0) + s["tx_bytes"].as_u64().unwrap_or(0);
                let bandwidth = match st.last_net.get(dev) {
                    Some(last) => round2((total(stats) as f64 - total(last) as f64) / time_diff),
                    None => 0.0,
                };
                stats["bandwidth"] = json!(bandwidth);
            }
            st.last_net = net.clone();
            update_cpu_stats(&mut st);
            update_system_memory(&mut st);
            let result = json!({"time": unix_time(), "cpu_usage": usage, "memory": memory, "mem_units": units});
            if st.queue.len() == REPORT_QUEUE_SIZE {
                st.queue.pop_front();
            }
            st.queue.push_back(result.clone());
            json!({
                "moonraker_stats": result,
                "cpu_temp": cpu_temp,
                "network": net,
                "system_cpu_usage": st.cpu_usage,
                "system_memory": st.memory_usage,
                "websocket_connections": app.client_count(),
            })
        };
        app.notify("proc_stat_update", vec![notification]);
        if sequence.is_multiple_of(THROTTLE_CHECK_INTERVAL)
            && let Some(ts) = ps.throttled_state().await
        {
            let bits = ts["bits"].as_u64().unwrap_or(0) as u32;
            let changed = {
                let mut st = ps.stats();
                if bits & !st.total_throttled != 0 {
                    tracing::info!("CPU Throttled Flags: {}", ts["flags"]);
                }
                let changed = bits != st.last_throttled;
                st.last_throttled = bits;
                st.total_throttled |= bits;
                changed
            };
            if changed {
                app.notify("cpu_throttled", vec![ts]);
            }
        }
        if sequence.is_multiple_of(crate::machine::NETWORK_UPDATE_SEQUENCE) && sequence > 0 {
            crate::machine::update_network(&app).await;
        }
        last_update = now;
        last_proc = proc_time;
        sequence += 1;
        tokio::time::sleep_until((now + STAT_UPDATE_TIME).into()).await;
    }
}

/// Python's `round(x, 2)`.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn seconds(ts: rustix::time::Timespec) -> f64 {
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Python's `time.process_time()`: CPU time used by this process.
fn process_time() -> f64 {
    seconds(rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime))
}

fn unix_time() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default()
}

/// orrery's resident memory in kB. Moonraker reads `Rss` from `/proc/self/smaps_rollup`, which
/// makes the kernel walk every memory mapping; `/proc/self/statm` has the same count for much
/// less work, and this runs every second.
fn memory_usage() -> (Option<u64>, Option<String>) {
    let Ok(text) = std::fs::read_to_string("/proc/self/statm") else { return (None, None) };
    let Some(pages) = text.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()) else {
        return (None, None);
    };
    (Some(pages * rustix::param::page_size() as u64 / 1024), Some("kB".into()))
}

fn net_stats() -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string("/proc/net/dev") else { return Map::new() };
    parse_net_dev(&text)
}

fn parse_net_dev(text: &str) -> Map<String, Value> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w+):(.+)").expect("valid regex"));
    let line = &*LINE;
    let mut out = Map::new();
    for c in line.captures_iter(text) {
        let fields: Vec<u64> = c[2].split_whitespace().filter_map(|f| f.parse().ok()).collect();
        if fields.len() < 12 {
            // Moonraker gives up on the whole file when a line is short.
            return Map::new();
        }
        out.insert(
            c[1].to_owned(),
            json!({
                "rx_bytes": fields[0],
                "tx_bytes": fields[8],
                "rx_packets": fields[1],
                "tx_packets": fields[9],
                "rx_errs": fields[2],
                "tx_errs": fields[10],
                "rx_drop": fields[3],
                "tx_drop": fields[11],
            }),
        );
    }
    out
}

/// System memory in kB, from `/proc/meminfo`.
fn update_system_memory(st: &mut Stats) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else { return };
    let field = |name: &str| {
        let line = text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))?;
        line.split_whitespace().next()?.parse::<u64>().ok()
    };
    if let (Some(total), Some(available)) = (field("MemTotal"), field("MemAvailable")) {
        st.memory_usage.insert("total".into(), json!(total));
        st.memory_usage.insert("available".into(), json!(available));
        st.memory_usage.insert("used".into(), json!(total - available));
    }
}

/// Each CPU's use since the last update, from `/proc/stat`.
fn update_cpu_stats(st: &mut Stats) {
    let Ok(text) = std::fs::read_to_string("/proc/stat") else { return };
    st.cpu_usage = cpu_usage(&text, &mut st.last_cpu);
}

fn cpu_usage(text: &str, last: &mut HashMap<String, (u64, u64)>) -> Map<String, Value> {
    let mut usage = Map::new();
    for line in text.lines().filter(|l| l.starts_with("cpu")) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(idle) = parts.get(4).and_then(|p| p.parse::<u64>().ok()) else { continue };
        let sum: u64 = parts[1..].iter().filter_map(|p| p.parse::<u64>().ok()).sum();
        let name = parts[0].to_owned();
        if let Some((last_sum, last_idle)) = last.get(&name) {
            let delta = sum.saturating_sub(*last_sum);
            let used = delta.saturating_sub(idle.saturating_sub(*last_idle));
            if delta > 0 {
                usage.insert(name.clone(), json!(round2(100.0 * used as f64 / delta as f64)));
            }
        }
        last.insert(name, (sum, idle));
    }
    usage
}

/// The CPU temperature file: an Intel, AMD, or Raspberry Pi sensor, or the first thermal zone.
fn cpu_thermal_file() -> PathBuf {
    if let Ok(dir) = std::fs::read_dir(HWMON_ROOT_PATH) {
        for entry in dir.flatten() {
            let path = entry.path();
            let Ok(name) = std::fs::read_to_string(path.join("name")) else { continue };
            if let Some((_, platform)) = HWMON_PLATFORMS.iter().find(|(n, _)| *n == name.trim()) {
                tracing::info!("Monitoring temperature for {platform} CPU");
                return path.join("temp1_input");
            }
        }
    }
    tracing::info!("Monitoring temperature using default thermal zone");
    PathBuf::from(TEMPERATURE_PATH)
}

/// Asks the Raspberry Pi firmware for its throttle state through the VideoCore mailbox, as
/// Moonraker's `VCGenCmd` does. This avoids starting the `vcgencmd` program every ten seconds.
mod vcgencmd {
    use std::sync::atomic::{AtomicBool, Ordering};

    const MAX_STRING_SIZE: usize = 1024;
    const GET_RESULT_CMD: u32 = 0x0003_0080;
    /// `_IOWR(100, 0, char *)`.
    const MAILBOX_REQUEST: rustix::ioctl::Opcode = rustix::ioctl::opcode::read_write::<*mut u8>(100, 0);
    static ERR_LOGGED: AtomicBool = AtomicBool::new(false);

    /// The mailbox message: a header, one tag with the command, and the end tag.
    #[repr(C)]
    struct Message {
        size: u32,
        code: u32,
        tag: u32,
        buffer_size: u32,
        request_size: u32,
        result: u32,
        text: [u8; MAX_STRING_SIZE],
        end: u32,
    }

    /// Returns the firmware's answer, such as `throttled=0x50000`.
    pub fn get_throttled() -> Option<String> {
        let fd = match std::fs::OpenOptions::new().read(true).write(true).open(super::VCIO_PATH) {
            Ok(f) => f,
            Err(e) => return log_error(e),
        };
        let mut msg = Message {
            size: size_of::<Message>() as u32,
            code: 0,
            tag: GET_RESULT_CMD,
            buffer_size: MAX_STRING_SIZE as u32,
            request_size: 0,
            result: 0,
            text: [0; MAX_STRING_SIZE],
            end: 0,
        };
        let cmd = b"get_throttled";
        msg.text[..cmd.len()].copy_from_slice(cmd);
        // SAFETY: the opcode matches the driver's request, and `msg` is the buffer the firmware
        // reads and fills, laid out as Moonraker packs it.
        let request = unsafe { rustix::ioctl::Updater::<MAILBOX_REQUEST, Message>::new(&mut msg) };
        // SAFETY: as above; the file is the VideoCore mailbox device.
        if let Err(e) = unsafe { rustix::ioctl::ioctl(&fd, request) } {
            return log_error(e.into());
        }
        if msg.result != 0 {
            tracing::info!("vcgencmd returned {}", msg.result);
        }
        let len = msg.text.iter().position(|&b| b == 0).unwrap_or(0);
        (len > 0).then(|| String::from_utf8_lossy(&msg.text[..len]).into_owned())
    }

    fn log_error(e: std::io::Error) -> Option<String> {
        if !ERR_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::warn!("VCIO vcgencmd failed: {e}");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usage_between_updates() {
        let mut last = HashMap::new();
        assert!(cpu_usage("cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\n", &mut last).is_empty());
        let usage = cpu_usage("cpu  150 0 150 900 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\n", &mut last);
        assert_eq!(usage["cpu"], json!(50.0));
        assert!(!usage.contains_key("cpu0"));
    }

    #[test]
    fn net_dev() {
        let text = "Inter-|   Receive\n face |bytes\n  eth0: 1000 10 1 2 0 0 0 0 2000 20 3 4 0 0 0 0\n";
        let net = parse_net_dev(text);
        assert_eq!(net["eth0"]["tx_bytes"], json!(2000));
        assert_eq!(net["eth0"]["tx_drop"], json!(4));
    }
}
