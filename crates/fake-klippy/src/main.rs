use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fake_klippy::{Capture, FakeKlippy, Script};
use orrery_proto::klippy;
use serde_json::{Map, Value, json};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(about = "Scriptable stand-in for Klipper's unix socket API")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a script on a unix socket until interrupted.
    Serve {
        /// YAML script to serve.
        #[arg(long)]
        script: PathBuf,
        /// Socket to listen on, as given to Moonraker's `klippy_uds_address`.
        #[arg(long)]
        socket: PathBuf,
        /// Report this directory as `[virtual_sdcard] path`.
        #[arg(long)]
        sdcard_path: Option<String>,
    },
    /// Record a running Klipper's state into a capture file that scripts can build on.
    Capture {
        /// Klipper's API socket (`klippy.py -a <socket>`).
        #[arg(long)]
        socket: PathBuf,
        /// Where to write the capture JSON.
        #[arg(long)]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    match Cli::parse().command {
        Command::Serve { script, socket, sdcard_path } => {
            let mut script = Script::from_file(&script)?;
            if let Some(path) = sdcard_path {
                script.set_sdcard_path(&path);
            }
            let _fake = FakeKlippy::start(&socket, script).await.context("starting fake klippy")?;
            eprintln!("fake-klippy: listening on {}", socket.display());
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Command::Capture { socket, out } => {
            let capture = capture(&socket).await?;
            let mut text = serde_json::to_string_pretty(&capture)?;
            text.push('\n');
            std::fs::write(&out, text).with_context(|| format!("writing {}", out.display()))?;
            eprintln!(
                "fake-klippy: captured {} objects and {} endpoints to {}",
                capture.objects.len(),
                capture.endpoints.len(),
                out.display()
            );
            Ok(())
        }
    }
}

async fn capture(socket: &PathBuf) -> Result<Capture> {
    let stream = UnixStream::connect(socket).await.with_context(|| format!("connecting to {}", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut next_id = 0;
    let mut call = async |method: &str, params: Value| -> Result<Value> {
        next_id += 1;
        klippy::write_message(&mut write, &json!({"id": next_id, "method": method, "params": params})).await?;
        loop {
            let msg = klippy::read_message(&mut reader).await?.context("klipper closed the socket")?;
            if msg.get("id") != Some(&json!(next_id)) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                bail!("{method}: {err}");
            }
            return Ok(msg["result"].clone());
        }
    };

    let mut info = as_map(call("info", json!({})).await?);
    if info.get("state").and_then(Value::as_str) != Some("ready") {
        bail!("klipper isn't ready: {}", info.get("state_message").unwrap_or(&Value::Null));
    }
    // Strip details of the machine the capture was made on.
    for (key, value) in [
        ("hostname", "fake-klippy"),
        ("klipper_path", "/opt/klipper"),
        ("python_path", "/opt/klippy-env/bin/python"),
        ("log_file", "/tmp/klippy.log"),
        ("config_file", "/opt/printer_data/config/printer.cfg"),
        ("cpu_info", "4 core ARMv8 Processor rev 3 (v8l)"),
    ] {
        if info.contains_key(key) {
            info.insert(key.into(), json!(value));
        }
    }
    // Replaced with the fake's own pid at runtime; fixed here so recaptures diff cleanly.
    info.insert("process_id".into(), json!(4242));
    let endpoints = call("list_endpoints", json!({})).await?["endpoints"]
        .as_array()
        .context("list_endpoints: no endpoints")?
        .iter()
        .filter_map(|e| e.as_str().map(String::from))
        .collect();
    let names = call("objects/list", json!({})).await?["objects"].clone();
    let requested: Map<String, Value> = names
        .as_array()
        .context("objects/list: no objects")?
        .iter()
        .filter_map(|n| n.as_str().map(|n| (n.to_owned(), Value::Null)))
        .collect();
    let mut objects = as_map(call("objects/query", json!({"objects": requested})).await?["status"].clone());
    fake_klippy::script::set_sdcard_path(&mut objects, "/opt/printer_data/gcodes");
    let gcode_help = as_map(call("gcode/help", json!({})).await?);
    Ok(Capture { info, endpoints, objects, gcode_help })
}

fn as_map(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
