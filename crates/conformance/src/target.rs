//! Starting the server under test in a fresh data directory.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use tokio::process::{Child, Command};

/// Which server to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    /// Upstream Moonraker from `conformance/.work/ref-venv` (see `just ref-setup`).
    Reference,
    /// Our server, from `target/debug/orrery` or `$ORRERY_BIN`.
    Orrery,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Orrery => "orrery",
        }
    }
}

/// Repository locations the runner needs.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Self { root: root.canonicalize().unwrap_or(root) }
    }

    pub fn conformance(&self) -> PathBuf {
        self.root.join("conformance")
    }

    pub fn work(&self) -> PathBuf {
        self.conformance().join(".work")
    }
}

/// A `printer_data`-style directory: `config/`, `logs/`, `gcodes/`, `database/`, `comms/`.
pub struct DataDir {
    pub dir: tempfile::TempDir,
}

impl DataDir {
    pub fn create(paths: &Paths, label: &str) -> Result<Self> {
        let runs = paths.work().join("runs");
        std::fs::create_dir_all(&runs)?;
        // The name stays short so the Klippy socket path fits in a unix socket address; the
        // label goes in a file instead.
        let dir = tempfile::Builder::new().prefix("r-").tempdir_in(&runs)?;
        for sub in ["config", "logs", "gcodes", "database", "comms", "tmp"] {
            std::fs::create_dir(dir.path().join(sub))?;
        }
        std::fs::write(dir.path().join("case.txt"), format!("{label}\n"))?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Klippy's socket. Kept short: unix socket paths are limited to 107 bytes.
    pub fn klippy_socket(&self) -> PathBuf {
        self.path().join("comms/klippy.sock")
    }

    /// Writes `moonraker.conf` from the base template plus a case's extra config.
    /// Writes a case's `files` into the data directory.
    pub fn write_files(&self, files: &Map<String, Value>) -> Result<()> {
        for (rel, content) in files {
            let rel = Path::new(rel);
            if rel.is_absolute() || rel.components().any(|c| c == std::path::Component::ParentDir) {
                bail!("files: {} must be a path inside the data directory", rel.display());
            }
            let Some(text) = content.as_str() else {
                bail!("files: the content of {} must be a string", rel.display());
            };
            let path = self.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    pub fn write_config(&self, paths: &Paths, port: u16, extra: Option<&str>) -> Result<()> {
        let template_path = paths.conformance().join("fixtures/moonraker/base.conf");
        let template =
            std::fs::read_to_string(&template_path).with_context(|| format!("reading {}", template_path.display()))?;
        let mut conf = template
            .replace("${port}", &port.to_string())
            .replace("${klippy_socket}", &self.klippy_socket().to_string_lossy())
            .replace("${data_path}", &self.path().to_string_lossy());
        if let Some(extra) = extra {
            // A section in the case's config replaces the base config's section of the same name.
            for name in section_names(extra) {
                conf = remove_section(&conf, &name);
            }
            conf.push('\n');
            conf.push_str(extra);
        }
        std::fs::write(self.path().join("config/moonraker.conf"), conf)?;
        Ok(())
    }
}

fn section_names(conf: &str) -> Vec<String> {
    conf.lines()
        .filter_map(|l| l.trim().strip_prefix('[').and_then(|l| l.strip_suffix(']')))
        .map(str::to_owned)
        .collect()
}

/// Removes `[name]` and its lines, up to the next section header.
fn remove_section(conf: &str, name: &str) -> String {
    let header = format!("[{name}]");
    let mut out = String::new();
    let mut skipping = false;
    for line in conf.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping = trimmed == header;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A running server. Killed on drop.
pub struct Server {
    child: Child,
    pub port: u16,
    pub kind: TargetKind,
}

impl Server {
    pub async fn start(kind: TargetKind, paths: &Paths, data: &DataDir, port: u16) -> Result<Self> {
        let log = std::fs::File::create(data.path().join("logs/stdout.log"))?;
        let mut cmd = match kind {
            TargetKind::Reference => {
                let python = paths.work().join("ref-venv/bin/python");
                if !python.exists() {
                    bail!("{} not found; run `just ref-setup` first", python.display());
                }
                let mut cmd = Command::new(python);
                cmd.args(["-m", "moonraker"]).env("GIT_CEILING_DIRECTORIES", paths.work());
                cmd
            }
            TargetKind::Orrery => {
                let bin = std::env::var_os("ORRERY_BIN")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| paths.root.join("target/debug/orrery"));
                if !bin.exists() {
                    bail!("{} not found; run `cargo build -p orrery` or set ORRERY_BIN", bin.display());
                }
                Command::new(bin)
            }
        };
        cmd.arg("-d").arg(data.path()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        // Each server gets its own temp folder. Moonraker names upload temp files after the
        // current second of the monotonic clock, so servers running side by side would overwrite
        // each other's uploads.
        cmd.env("TMPDIR", data.path().join("tmp"));
        cmd.kill_on_drop(true);
        let child = cmd.spawn().with_context(|| format!("starting {}", kind.as_str()))?;
        let mut server = Self { child, port, kind };
        server.wait_listening(Duration::from_secs(20)).await?;
        Ok(server)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/websocket", self.port)
    }

    async fn wait_listening(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let client = reqwest::Client::new();
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("{} exited during startup ({status})", self.kind.as_str());
            }
            if let Ok(resp) = client.get(format!("{}/server/info", self.base_url())).send().await
                && resp.status().is_success()
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("{} didn't answer /server/info within {timeout:?}", self.kind.as_str());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Finds a free TCP port on localhost. Racy, but good enough for local test runs.
pub fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Real Klipper from `conformance/.work/klipper-src`, running the fixture printer.cfg against the
/// Linux host-MCU data dictionary. With `-o /dev/null` and `-d`, Klipper writes MCU commands to
/// a file instead of talking to hardware. Killed on drop.
pub struct RealKlippy {
    _child: Child,
}

impl RealKlippy {
    pub async fn start(paths: &Paths, data: &DataDir) -> Result<Self> {
        let work = paths.work();
        let fixtures = paths.conformance().join("fixtures/klipper");
        let python = work.join("klipper-venv/bin/python");
        if !python.exists() {
            bail!("{} not found; run `just klipper-setup` first", python.display());
        }
        let cfg = std::fs::read_to_string(fixtures.join("printer.cfg"))?
            .replace("${gcode_path}", &data.path().join("gcodes").to_string_lossy());
        let cfg_path = data.path().join("config/printer.cfg");
        std::fs::write(&cfg_path, cfg)?;
        let socket = data.klippy_socket();
        let log = std::fs::File::create(data.path().join("logs/klippy-stdout.log"))?;
        let mut cmd = Command::new(python);
        // Klipper asks git for its version. Without a ceiling, git finds this repository around
        // the copied source and Klipper reports our commit instead of klippy/.version.
        cmd.env("GIT_CEILING_DIRECTORIES", &work)
            .arg(work.join("klipper-src/klippy/klippy.py"))
            .arg(&cfg_path)
            .args(["-o", "/dev/null", "-d"])
            .arg(fixtures.join("klipper.dict"))
            .arg("-a")
            .arg(&socket)
            .arg("-I")
            .arg(data.path().join("comms/klippy.serial"))
            .arg("-l")
            .arg(data.path().join("logs/klippy.log"))
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("starting klippy")?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !socket.exists() {
            if let Some(status) = child.try_wait()? {
                bail!("klippy exited during startup ({status}); see logs/klippy.log");
            }
            if Instant::now() > deadline {
                bail!("klippy didn't create its socket within 20s");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(Self { _child: child })
    }
}
