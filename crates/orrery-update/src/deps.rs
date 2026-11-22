//! An entry's dependencies: the Python packages in its `requirements` file, and the system
//! packages in its `system_dependencies` file or, as the older way, in its `install_script`.
//! They're read as Moonraker reads them (`update_manager/app_deploy.py`,
//! `utils/pip_utils.py`, `utils/sysdeps_parser.py`). orrery never runs the install script.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::packagekit::Messages;

/// How long one `pip install` may run, and how often it's tried, as in Moonraker.
const PIP_TIMEOUT: Duration = Duration::from_secs(1200);
const PIP_ATTEMPTS: usize = 3;

/// Where an entry's dependencies are listed. Paths are relative to `path`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DepFiles {
    pub path: PathBuf,
    pub virtualenv: Option<PathBuf>,
    pub pip_env: Vec<(String, String)>,
    pub requirements: Option<String>,
    pub system_dependencies: Option<String>,
    pub install_script: Option<String>,
}

/// What an entry depends on at one point in time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deps {
    pub packages: Vec<String>,
    pub modules: Vec<String>,
}

impl Deps {
    /// Reads the entry's dependency files as they are now.
    pub async fn read(opts: &DepFiles) -> Self {
        Self { packages: system_packages(opts).await, modules: python_modules(opts).await }
    }

    /// What `after` has that `self` didn't, or all of `after` when `force` is set.
    pub fn added(&self, after: &Deps, force: bool) -> Deps {
        let new = |a: &[String], b: &[String]| -> Vec<String> {
            a.iter().filter(|x| force || !b.contains(x)).cloned().collect()
        };
        Deps { packages: new(&after.packages, &self.packages), modules: new(&after.modules, &self.modules) }
    }
}

/// The requirements file, when the entry has a virtualenv: Moonraker reads `requirements`
/// only then.
pub fn requirements_file(opts: &DepFiles) -> Option<PathBuf> {
    opts.virtualenv.as_ref()?;
    opts.requirements.as_ref().map(|r| opts.path.join(r))
}

async fn python_modules(opts: &DepFiles) -> Vec<String> {
    let Some(path) = requirements_file(opts) else { return Vec::new() };
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => read_requirements(&text),
        Err(e) => {
            tracing::info!("Can't read the Python requirements in {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Moonraker's `read_requirements_file`: each line that isn't empty, a comment, or an option,
/// without its trailing comment.
pub fn read_requirements(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', '-']))
        .map(|l| {
            let end = l.char_indices().find(|&(i, c)| c == '#' && l[..i].ends_with(char::is_whitespace));
            end.map_or(l, |(i, _)| l[..i].trim()).to_owned()
        })
        .collect()
}

async fn system_packages(opts: &DepFiles) -> Vec<String> {
    if let Some(file) = &opts.system_dependencies {
        let path = opts.path.join(file);
        let parsed = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
        return match parsed {
            Ok(deps) => Distro::read().packages(&deps),
            Err(e) => {
                tracing::info!("Can't read the system dependencies in {}: {e}", path.display());
                Vec::new()
            }
        };
    }
    let Some(script) = &opts.install_script else { return Vec::new() };
    let path = opts.path.join(script);
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => {
            let packages = pkglist(&text);
            if packages.is_empty() {
                tracing::info!("No packages found in script: {}", path.display());
            }
            packages
        }
        Err(e) => {
            tracing::info!("Failed to open install script {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// The packages in an install script's `PKGLIST="…"` lines. Moonraker strips the characters
/// of `${PKGLIST}` from the start of each list, which removes a leading `${PKGLIST}`.
pub fn pkglist(script: &str) -> Vec<String> {
    let mut packages = Vec::new();
    for line in script.lines() {
        let mut rest = line;
        while let Some(start) = rest.find("PKGLIST=\"") {
            let after = &rest[start + 9..];
            // Moonraker's `PKGLIST="(.*)"` is greedy: it runs to the last quote on the line.
            let Some(end) = after.rfind('"') else { break };
            let list = after[..end].trim_start_matches(|c| "${PKGLIST}".contains(c)).trim();
            packages.extend(list.split_whitespace().map(str::to_owned));
            rest = &after[end + 1..];
        }
    }
    packages
}

/// The Linux release, from `/etc/os-release`, and the settings that pick packages.
#[derive(Debug, Clone, Default)]
pub struct Distro {
    pub id: String,
    pub aliases: Vec<String>,
    pub version: Vec<Part>,
    pub vendor: String,
    pub exclusions: Vec<String>,
}

impl Distro {
    pub fn read() -> Self {
        let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let mut d = Self::from_os_release(&text);
        d.vendor = std::env::var("MOONRAKER_VENDOR").unwrap_or_default();
        if d.vendor.is_empty() && Path::new("/etc/rpi-issue").is_file() {
            d.vendor = "raspberry-pi".into();
        }
        d.exclusions = std::env::var("MOONRAKER_EXCLUDED_PKGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        d
    }

    pub fn from_os_release(text: &str) -> Self {
        let get = |key: &str| {
            text.lines()
                .filter_map(|l| l.trim().split_once('='))
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.trim().trim_matches(['"', '\'']).to_owned())
                .unwrap_or_default()
        };
        let version = get("VERSION_ID");
        Self {
            id: get("ID"),
            aliases: get("ID_LIKE").split_whitespace().map(str::to_owned).collect(),
            version: if version.is_empty() { Vec::new() } else { convert_version(&version) },
            ..Self::default()
        }
    }

    /// Moonraker's `parse_dependencies`: the list for this release's ID, or for the first of
    /// its `ID_LIKE` names that has one, with each package's condition checked.
    pub fn packages(&self, deps: &Value) -> Vec<String> {
        if self.id.is_empty() {
            tracing::info!("Failed to detect current distro ID, cannot parse dependencies");
            return Vec::new();
        }
        for id in std::iter::once(&self.id).chain(&self.aliases) {
            let Some(list) = deps[id.as_str()].as_array() else { continue };
            if list.is_empty() {
                continue;
            }
            return list.iter().filter_map(Value::as_str).filter_map(|spec| self.parse_spec(spec)).collect();
        }
        tracing::info!("Dependency data has no package definition for linux distro '{}'", self.id);
        Vec::new()
    }

    /// `name; condition`: the name, if the condition holds for this release.
    fn parse_spec(&self, spec: &str) -> Option<String> {
        let (name, condition) = match spec.split_once(';') {
            Some((n, c)) => (n.trim(), Some(c.trim())),
            None => (spec.trim(), None),
        };
        if name.is_empty() || self.exclusions.iter().any(|e| e == name) {
            tracing::info!("Package '{spec}' excluded by environment");
            return None;
        }
        let Some(condition) = condition else { return Some(name.to_owned()) };
        match self.holds(condition) {
            Some(true) => Some(name.to_owned()),
            Some(false) => None,
            None => {
                tracing::info!("Invalid requirement specifier: {spec}");
                None
            }
        }
    }

    /// Moonraker reads `and` and `or` from left to right, with no precedence. `None` for a
    /// condition it can't read.
    fn holds(&self, condition: &str) -> Option<bool> {
        let mut result = true;
        let mut op = "and";
        let mut rest = condition;
        loop {
            let next = [" and ", " or "].iter().filter_map(|o| rest.find(o).map(|i| (i, *o))).min();
            let (expr, after) = match next {
                Some((i, o)) => (&rest[..i], Some((o.trim(), &rest[i + o.len()..]))),
                None => (rest, None),
            };
            let value = self.compare(expr.trim())?;
            if op == "and" {
                result &= value;
            } else {
                result |= value;
            }
            match after {
                Some((o, r)) => {
                    op = o;
                    rest = r;
                }
                None => return Some(result),
            }
        }
    }

    fn compare(&self, expr: &str) -> Option<bool> {
        let ops = ["==", "!=", "<=", ">=", "<", ">"];
        let (i, op) =
            ops.iter().filter_map(|o| expr.find(o).map(|i| (i, *o))).min_by_key(|(i, o)| (*i, -(o.len() as i32)))?;
        let var = expr[..i].trim().to_ascii_lowercase();
        let right = expr[i + op.len()..].trim().trim_matches(['"', '\'']);
        let ordering = match var.as_str() {
            "distro_id" => Some(self.id.as_str().cmp(right)),
            "vendor" => Some(self.vendor.as_str().cmp(right)),
            "distro_version" => {
                if self.version.is_empty() {
                    return None;
                }
                let right = convert_version(right);
                // Python compares an int with a string only for equality.
                match compare_parts(&self.version, &right) {
                    Some(o) => Some(o),
                    None => return matches!(op, "!=").then_some(true).or(matches!(op, "==").then_some(false)),
                }
            }
            _ => return None,
        }?;
        Some(match op {
            "==" => ordering == Ordering::Equal,
            "!=" => ordering != Ordering::Equal,
            "<=" => ordering != Ordering::Greater,
            ">=" => ordering != Ordering::Less,
            "<" => ordering == Ordering::Less,
            _ => ordering == Ordering::Greater,
        })
    }
}

/// A part of a version: Moonraker splits `12.1-rc` into `(12, 1, "rc")`.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Int(u64),
    Text(String),
}

/// Moonraker's `_convert_version`.
pub fn convert_version(version: &str) -> Vec<Part> {
    let v = version.trim();
    if !v.starts_with(|c: char| c.is_ascii_digit()) {
        return vec![Part::Text(v.to_owned())];
    }
    v.split(['.', '-'])
        .map(|p| {
            if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
                p.parse().map_or_else(|_| Part::Text(p.into()), Part::Int)
            } else {
                Part::Text(p.into())
            }
        })
        .collect()
}

/// Python's tuple order. `None` where Python raises: an int against a string.
fn compare_parts(a: &[Part], b: &[Part]) -> Option<Ordering> {
    for (x, y) in a.iter().zip(b) {
        let o = match (x, y) {
            (Part::Int(x), Part::Int(y)) => x.cmp(y),
            (Part::Text(x), Part::Text(y)) => x.cmp(y),
            _ => return None,
        };
        if o != Ordering::Equal {
            return Some(o);
        }
    }
    Some(a.len().cmp(&b.len()))
}

/// Runs `<virtualenv>/bin/python -m pip install -U -r <requirements>`, and passes on each line
/// pip prints. It's tried three times, as in Moonraker.
pub async fn pip_install(opts: &DepFiles, requirements: &Path, on_line: &Messages) -> Result<(), String> {
    let venv = opts.virtualenv.as_ref().ok_or("The entry has no virtualenv")?;
    let python = venv.join("bin/python");
    if !python.is_file() {
        return Err(format!("There's no Python at {}", python.display()));
    }
    let mut last = String::new();
    for attempt in 1..=PIP_ATTEMPTS {
        let mut child = tokio::process::Command::new(&python)
            .args(["-m", "pip", "install", "-U", "-r"])
            .arg(requirements)
            .envs(opts.pip_env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Can't run {}: {e}", python.display()))?;
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let errors = tokio::spawn(async move {
            let mut text = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut text).await;
            text
        });
        let run = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    let _ = on_line.send(line.trim_end().to_owned());
                }
            }
            child.wait().await
        };
        let status = match tokio::time::timeout(PIP_TIMEOUT, run).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err(format!("pip ran for more than {} seconds", PIP_TIMEOUT.as_secs())),
        };
        let stderr = errors.await.unwrap_or_default();
        if status.success() {
            return Ok(());
        }
        last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_owned();
        tracing::info!("pip install failed, attempt {attempt} of {PIP_ATTEMPTS}: {last}");
    }
    Err(format!("pip install failed: {last}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requirements() {
        let text = "# comment\n-r other.txt\nnumpy>=1.2  # fast\n\nrequests\nzeroconf#notacomment\n";
        assert_eq!(read_requirements(text), ["numpy>=1.2", "requests", "zeroconf#notacomment"]);
    }

    #[test]
    fn install_script_lists() {
        let script =
            "PKGLIST=\"python3-numpy python3-matplotlib\"\nPKGLIST=\"${PKGLIST} libatlas-base-dev\"\necho PKGLIST\n";
        assert_eq!(pkglist(script), ["python3-numpy", "python3-matplotlib", "libatlas-base-dev"]);
    }

    fn distro(text: &str) -> Distro {
        Distro::from_os_release(text)
    }

    #[test]
    fn system_dependencies() {
        let trixie = distro("PRETTY_NAME=\"Armbian 25.11.2 trixie\"\nID=debian\nVERSION_ID=\"13\"\n");
        let ubuntu = distro("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.10\"\n");
        let deps = json!({"debian": [
            "packagekit",
            "wireless-tools; distro_id != 'ubuntu' or distro_version <= '24.04'",
            "iw; distro_id == 'ubuntu' and distro_version >= '24.10'",
            "python3-libcamera; vendor == 'raspberry-pi' and distro_version >= '11'",
            "bad; nonsense == 1",
        ]});
        assert_eq!(trixie.packages(&deps), ["packagekit", "wireless-tools"]);
        assert_eq!(ubuntu.packages(&deps), ["packagekit", "iw"]);
        let pi = Distro { vendor: "raspberry-pi".into(), ..trixie.clone() };
        assert_eq!(pi.packages(&deps), ["packagekit", "wireless-tools", "python3-libcamera"]);
        let excluded = Distro { exclusions: vec!["packagekit".into()], ..trixie.clone() };
        assert_eq!(excluded.packages(&deps), ["wireless-tools"]);
        assert!(trixie.packages(&json!({"fedora": ["x"]})).is_empty());
    }

    #[test]
    fn versions_compare_like_python() {
        let v = convert_version;
        assert_eq!(v("24.04"), [Part::Int(24), Part::Int(4)]);
        assert_eq!(compare_parts(&v("24.10"), &v("24.04")), Some(Ordering::Greater));
        assert_eq!(compare_parts(&v("13"), &v("13.1")), Some(Ordering::Less));
        assert_eq!(compare_parts(&v("13-rc"), &v("13.1")), None);
        assert_eq!(v("sid"), [Part::Text("sid".into())]);
    }

    fn venv_opts(dir: &Path, script: &str) -> DepFiles {
        use std::os::unix::fs::PermissionsExt;
        let python = dir.join("env/bin/python");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, script).unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
        DepFiles {
            path: dir.to_path_buf(),
            virtualenv: Some(dir.join("env")),
            pip_env: vec![("PIP_TEST".into(), "yes".into())],
            requirements: Some("requirements.txt".into()),
            system_dependencies: None,
            install_script: None,
        }
    }

    #[tokio::test]
    async fn pip_runs_in_the_virtualenv() {
        let dir = tempfile::tempdir().unwrap();
        let opts = venv_opts(dir.path(), "#!/bin/sh\necho \"args: $*\"\necho \"env: $PIP_TEST\"\n");
        let req = requirements_file(&opts).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pip_install(&opts, &req, &tx).await.unwrap();
        drop(tx);
        let mut lines = Vec::new();
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        assert_eq!(lines, [format!("args: -m pip install -U -r {}", req.display()), "env: yes".into()]);

        // A failing pip is tried three times, and its last error line is the message.
        let calls = dir.path().join("calls");
        let script = format!("#!/bin/sh\necho x >> {}\necho 'ERROR: no such package' >&2\nexit 1\n", calls.display());
        let opts = venv_opts(dir.path(), &script);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(pip_install(&opts, &req, &tx).await.unwrap_err(), "pip install failed: ERROR: no such package");
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 3);
        // Without a virtualenv, there's nothing to install into.
        assert!(requirements_file(&DepFiles { virtualenv: None, ..opts }).is_none());
    }

    #[test]
    fn added_dependencies() {
        let before = Deps { packages: vec!["a".into()], modules: vec!["x==1".into()] };
        let after = Deps { packages: vec!["a".into(), "b".into()], modules: vec!["x==2".into()] };
        let added = before.added(&after, false);
        assert_eq!((added.packages, added.modules), (vec!["b".to_owned()], vec!["x==2".to_owned()]));
        assert_eq!(before.added(&after, true), after);
    }
}
