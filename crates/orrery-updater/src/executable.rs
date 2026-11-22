//! `executable` entries: a program from a project's GitHub releases, with a
//! `release_info.json` beside it, read and updated as Moonraker's `NetDeploy` does
//! (`update_manager/net_deploy.py`). The new program is downloaded beside the old one and
//! renamed over it, so a failed download leaves the old one as it was.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::config::{Channel, Entry, ExecOptions};
use crate::packagekit::Messages;

const GITHUB_API: &str = "https://api.github.com/";
const ATTEMPTS: usize = 3;
const RETRY_PAUSE: Duration = Duration::from_millis(500);

/// A release's file to download.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub url: String,
    pub content_type: String,
    pub size: u64,
}

/// What the database keeps for an executable entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    /// The newest release's tag, or `?`.
    pub remote_version: String,
    pub asset: Option<Asset>,
    pub prerelease: bool,
    /// The request and the `ETag` GitHub gave for it: asking again with it costs nothing
    /// from the rate limit when nothing changed.
    pub etag: Option<(String, String)>,
    pub last_error: String,
    pub rollback_version: String,
    pub rollback_repo: String,
    /// For orrery's own entry: the release's `SHA256SUMS`.
    pub checksums: Option<Asset>,
    /// For orrery's own entry: the version an update or a rollback put in place, until orrery
    /// restarts with it.
    pub expected_version: Option<String>,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            remote_version: "?".into(),
            asset: None,
            prerelease: false,
            etag: None,
            last_error: String::new(),
            rollback_version: "?".into(),
            rollback_repo: "?".into(),
            checksums: None,
            expected_version: None,
        }
    }
}

impl Saved {
    pub fn from_json(v: &Value) -> Self {
        let text = |k: &str, default: &str| v[k].as_str().unwrap_or(default).to_owned();
        let asset_at = |a: &Value| {
            a["url"].as_str().map(|url| Asset {
                url: url.to_owned(),
                content_type: a["content_type"].as_str().unwrap_or("?").to_owned(),
                size: a["size"].as_u64().unwrap_or(0),
            })
        };
        let asset = asset_at(&v["asset"]);
        let etag = match (v["etag"][0].as_str(), v["etag"][1].as_str()) {
            (Some(url), Some(tag)) => Some((url.to_owned(), tag.to_owned())),
            _ => None,
        };
        Self {
            remote_version: text("remote_version", "?"),
            asset,
            prerelease: v["prerelease"].as_bool().unwrap_or(false),
            etag,
            last_error: text("last_error", ""),
            rollback_version: text("rollback_version", "?"),
            rollback_repo: text("rollback_repo", "?"),
            checksums: asset_at(&v["checksums"]),
            expected_version: v["expected_version"].as_str().map(str::to_owned),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "remote_version": self.remote_version,
            "asset": self.asset.as_ref().map(|a| json!({"url": a.url, "content_type": a.content_type, "size": a.size})),
            "prerelease": self.prerelease,
            "etag": self.etag.as_ref().map(|(u, t)| json!([u, t])),
            "last_error": self.last_error,
            "rollback_version": self.rollback_version,
            "rollback_repo": self.rollback_repo,
            "checksums": self.checksums.as_ref().map(|a| json!({"url": a.url, "content_type": a.content_type, "size": a.size})),
            "expected_version": self.expected_version,
        })
    }
}

/// What's installed, from `release_info.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub valid: bool,
    pub version: String,
    pub asset_name: Option<String>,
    /// `owner/name`: the one `release_info.json` names, which wins over the config's.
    pub repo: String,
    pub warnings: Vec<String>,
    pub anomalies: Vec<String>,
}

impl Installed {
    pub fn owner(&self) -> &str {
        self.repo.split_once('/').map_or("?", |(o, _)| o)
    }

    pub fn project(&self) -> &str {
        self.repo.split_once('/').map_or("?", |(_, n)| n)
    }
}

/// Moonraker's `_validate_release_info`.
pub async fn installed(opts: &ExecOptions) -> Installed {
    let mut i = Installed {
        valid: false,
        version: "?".into(),
        asset_name: None,
        repo: opts.repo.clone(),
        warnings: Vec::new(),
        anomalies: Vec::new(),
    };
    let path = &opts.path;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let repo_parent = path.ancestors().find(|p| p.join(".git").exists());
    if !path.is_dir() {
        i.warnings.push(format!("Location at option 'path: {}' is not a directory.", path.display()));
    } else if rustix::fs::access(path.as_path(), rustix::fs::Access::WRITE_OK).is_err() {
        i.warnings.push(format!("Location at option 'path: {}' is not writable.", path.display()));
    } else if let Some(parent) = repo_parent.filter(|p| Some(p.to_path_buf()) != home) {
        i.warnings.push(format!(
            "Location at option 'path: {}' is within a git repo. Found .git folder at '{}'",
            path.display(),
            parent.join(".git").display()
        ));
    } else {
        let info = tokio::fs::read_to_string(path.join("release_info.json"))
            .await
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let fields = info
            .as_ref()
            .and_then(|v| Some((v["project_name"].as_str()?, v["project_owner"].as_str()?, v["version"].as_str()?)));
        match fields {
            None => tracing::info!("Failed to load {}", path.join("release_info.json").display()),
            Some((project, owner, version)) => {
                i.valid = true;
                i.version = version.to_owned();
                i.asset_name = info.as_ref().and_then(|v| v["asset_name"].as_str()).map(str::to_owned);
                let detected = format!("{owner}/{project}");
                if opts.repo_pinned {
                    // orrery.toml's repo wins, so a fork's releases can take over.
                } else if opts.repo.is_empty() {
                    i.repo = detected;
                } else if !detected.eq_ignore_ascii_case(&opts.repo) {
                    i.anomalies.push(format!(
                        "Value at option 'repo: {}' does not match detected repo '{detected}', falling back to detected version.",
                        opts.repo
                    ));
                    i.repo = detected;
                }
                match &i.asset_name {
                    None => {
                        i.warnings.push("Executable types require the 'asset_name' field in release_info.json".into());
                        i.valid = false;
                    }
                    Some(name) if opts.programs.is_empty() && !path.join(name).exists() => {
                        i.warnings.push(format!("File {name} not found in configured path for executable type"));
                        i.valid = false;
                    }
                    Some(_) if !opts.programs.is_empty() => {
                        for program in opts.programs.iter().filter(|p| !path.join(p).is_file()) {
                            i.warnings.push(format!("File {program} not found in configured path for executable type"));
                            i.valid = false;
                        }
                    }
                    Some(_) => {}
                }
            }
        }
    }
    if !i.valid {
        i.warnings.push("Failed to validate installation".into());
    }
    i
}

/// The entry's object in `version_info`.
pub fn status(entry: &Entry, i: &Installed, saved: &Saved) -> Value {
    // What .previous/ holds wins: an update from a shell changes it without the database.
    let previous = match &entry.kind {
        crate::config::Kind::Executable(opts) => previous_version(&opts.path, &files(opts, i)),
        _ => None,
    };
    let rollback = match &previous {
        Some(v) => v.as_str(),
        None if saved.rollback_version == "?" => i.version.as_str(),
        None => saved.rollback_version.as_str(),
    };
    json!({
        "name": entry.name,
        "configured_type": "executable",
        "channel": entry.channel.as_str(),
        "debug_enabled": false,
        "channel_invalid": entry.channel_warning.is_some(),
        "is_valid": i.valid,
        "info_tags": entry.info_tags,
        "repo_name": i.project(),
        "owner": i.owner(),
        "version": i.version,
        "remote_version": saved.remote_version,
        "rollback_version": rollback,
        "last_error": saved.last_error,
        "warnings": i.warnings,
        "anomalies": if entry.report_anomalies { i.anomalies.clone() } else { Vec::new() },
    })
}

/// GitHub's rate limit, from the last answer's headers.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset: Option<f64>,
}

/// What a release request found.
pub enum Fetched {
    /// The release, and its `ETag`.
    Release(Value, Option<String>),
    /// The same as last time.
    NotModified,
}

/// GitHub's API, and downloads from it.
pub struct Github {
    http: reqwest::Client,
    api: String,
    rate: Arc<Mutex<Rate>>,
}

impl Github {
    /// `ORRERY_GITHUB_API` points the agent at another server, for tests.
    pub fn new() -> Result<Self, String> {
        let api = std::env::var("ORRERY_GITHUB_API").unwrap_or_else(|_| GITHUB_API.into());
        Self::with_api(&api)
    }

    pub fn with_api(api: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls_config()?)
            .user_agent(format!("orrery-updater/{}", orrery_build::VERSION))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("Can't set up the HTTP client: {e}"))?;
        let api = if api.ends_with('/') { api.to_owned() } else { format!("{api}/") };
        Ok(Self { http, api, rate: Arc::default() })
    }

    pub fn rate(&self) -> Rate {
        *self.rate.lock().expect("rate lock")
    }

    /// The release a channel follows, or the one tagged `tag`, as Moonraker's
    /// `_fetch_github_version` asks for it.
    pub async fn release(
        &self,
        repo: &str,
        channel: Channel,
        tag: Option<&str>,
        etag: Option<&(String, String)>,
    ) -> Result<Fetched, String> {
        let resource = match (tag, channel) {
            (Some(tag), _) => format!("repos/{repo}/releases/tags/{tag}"),
            (None, Channel::Stable) => format!("repos/{repo}/releases/latest"),
            (None, _) => format!("repos/{repo}/releases?per_page=1"),
        };
        let url = format!("{}{resource}", self.api);
        let rate = self.rate();
        if rate.remaining == Some(0)
            && let Some(reset) = rate.reset
            && now() < reset
        {
            return Err(format!("GitHub Rate Limit Reached\nRequest: {url}\nLimit Reset Time: {}", utc(reset)));
        }
        let etag = etag.filter(|(u, _)| *u == url).map(|(_, t)| t.clone());
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            let mut req =
                self.http.get(&url).header("Accept", "application/vnd.github.v3+json").timeout(Duration::from_secs(30));
            if let Some(tag) = &etag {
                req = req.header("If-None-Match", tag);
            }
            match req.send().await {
                Ok(resp) => {
                    self.note_rate(resp.headers());
                    let status = resp.status();
                    if status == reqwest::StatusCode::NOT_MODIFIED {
                        return Ok(Fetched::NotModified);
                    }
                    if status.is_success() {
                        let tag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned);
                        let body: Value = resp.json().await.map_err(|e| format!("GitHub's answer isn't JSON: {e}"))?;
                        // `releases?per_page=1` is a list of one.
                        let release = match body {
                            Value::Array(list) => list.into_iter().next().unwrap_or(Value::Null),
                            other => other,
                        };
                        return Ok(Fetched::Release(release, tag));
                    }
                    last = format!("GitHub answered {status} for {url}");
                    if !status.is_server_error() {
                        break;
                    }
                }
                Err(e) => last = format!("Can't reach GitHub at {url}: {}", error_chain(&e)),
            }
            if attempt < ATTEMPTS {
                tokio::time::sleep(RETRY_PAUSE).await;
            }
        }
        Err(last)
    }

    fn note_rate(&self, headers: &reqwest::header::HeaderMap) {
        let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
        if let (Some(limit), Some(remaining), Some(reset)) =
            (get("x-ratelimit-limit"), get("x-ratelimit-remaining"), get("x-ratelimit-reset"))
        {
            *self.rate.lock().expect("rate lock") =
                Rate { limit: Some(limit as u64), remaining: Some(remaining as u64), reset: Some(reset) };
        }
    }

    /// Downloads `asset` to `dest`, and reports the progress as Moonraker words it.
    pub async fn download(&self, asset: &Asset, dest: &Path, application: &str, out: &Messages) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let fail = |e: reqwest::Error| format!("Can't download {}: {}", asset.url, error_chain(&e));
        let mut resp = self
            .http
            .get(&asset.url)
            .header("Accept", if asset.content_type == "?" { "application/octet-stream" } else { &asset.content_type })
            .send()
            .await
            .map_err(fail)?;
        if !resp.status().is_success() {
            return Err(format!("Can't download {}: the server answered {}", asset.url, resp.status()));
        }
        let total = if asset.size > 0 { asset.size } else { resp.content_length().unwrap_or(0) };
        let mut file =
            tokio::fs::File::create(dest).await.map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
        let (mut done, mut shown) = (0u64, None);
        while let Some(chunk) = resp.chunk().await.map_err(fail)? {
            file.write_all(&chunk).await.map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
            done += chunk.len() as u64;
            // Every tenth of the way, not every chunk, and nothing for a small file, such as a
            // checksum file.
            if total >= 64 * 1024 {
                let percent = (done * 100 / total).min(100);
                if shown.is_none_or(|s| percent >= s + 10 || (percent == 100 && s != 100)) {
                    shown = Some(percent);
                    let _ = out.send(format!(
                        "Downloading {application}: {} KiB / {} KiB [{percent}%]",
                        done / 1024,
                        total / 1024
                    ));
                }
            }
        }
        file.sync_all().await.map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
        if asset.size > 0 && done != asset.size {
            return Err(format!("The download of {} has {done} bytes, not {}", asset.url, asset.size));
        }
        Ok(())
    }
}

/// The release's file named `name`.
pub fn asset_named(release: &Value, name: &str) -> Option<Asset> {
    let assets = release["assets"].as_array()?;
    let a = assets.iter().find(|a| a["name"] == name)?;
    Some(Asset {
        url: a["browser_download_url"].as_str()?.to_owned(),
        content_type: a["content_type"].as_str().unwrap_or("?").to_owned(),
        size: a["size"].as_u64().unwrap_or(0),
    })
}

/// The tag, the asset named `asset_name` (or the first one), and whether it's a pre-release,
/// as Moonraker's `_get_remote_version` reads a release.
pub fn read_release(release: &Value, asset_name: Option<&str>) -> (String, Option<Asset>, bool) {
    let version = release["tag_name"].as_str().or(release["name"].as_str()).filter(|v| !v.is_empty()).unwrap_or("?");
    let assets = release["assets"].as_array().map(Vec::as_slice).unwrap_or_default();
    let asset = match asset_name {
        Some(name) => assets.iter().find(|a| a["name"] == name),
        None => assets.first(),
    };
    if asset.is_none() {
        tracing::info!("Asset '{}' not found in release {version}", asset_name.unwrap_or("?"));
    }
    let asset = asset.and_then(|a| {
        Some(Asset {
            url: a["browser_download_url"].as_str()?.to_owned(),
            content_type: a["content_type"].as_str().unwrap_or("?").to_owned(),
            size: a["size"].as_u64().unwrap_or(0),
        })
    });
    (version.to_owned(), asset, release["prerelease"].as_bool().unwrap_or(false))
}

/// Checks GitHub for the channel's release, and keeps what it finds in `saved`.
pub async fn refresh(gh: &Github, entry: &Entry, i: &Installed, saved: &mut Saved) {
    if !i.valid {
        tracing::info!("{}Invalid Installation, aborting remote refresh", entry.prefix());
        return;
    }
    match gh.release(&i.repo, entry.channel, None, saved.etag.as_ref()).await {
        Ok(Fetched::NotModified) => saved.last_error.clear(),
        Ok(Fetched::Release(release, etag)) => {
            (saved.remote_version, saved.asset, saved.prerelease) = read_release(&release, i.asset_name.as_deref());
            saved.checksums = checksums_of(entry, &release);
            let url = format!("{}{}", gh.api, release_resource(&i.repo, entry.channel));
            saved.etag = etag.map(|t| (url, t));
            saved.last_error.clear();
        }
        Err(e) => {
            tracing::info!("{}{e}", entry.prefix());
            saved.last_error = e;
        }
    }
}

/// The release's checksum file, when the entry names one.
pub fn checksums_of(entry: &Entry, release: &Value) -> Option<Asset> {
    let crate::config::Kind::Executable(opts) = &entry.kind else { return None };
    asset_named(release, opts.checksums.as_deref()?)
}

fn release_resource(repo: &str, channel: Channel) -> String {
    match channel {
        Channel::Stable => format!("repos/{repo}/releases/latest"),
        _ => format!("repos/{repo}/releases?per_page=1"),
    }
}

/// The files an update replaces, in order: the programs of an archive, or the program the
/// asset is.
pub fn files(opts: &ExecOptions, i: &Installed) -> Vec<String> {
    if opts.programs.is_empty() { i.asset_name.iter().cloned().collect() } else { opts.programs.clone() }
}

/// Installs `asset` over the entry's files, and records `version` in `release_info.json`.
///
/// The download goes to a hidden folder beside the files, `.<entry>.update`. There it's
/// checked against `checksums` when the entry names a checksum file, unpacked when it's an
/// archive of `programs`, and run as the preflight. Then the current files are hard-linked
/// into `.previous/`, and each new one is renamed over the old one, in order. Before the
/// renames, a failure leaves every file as it was.
#[allow(clippy::too_many_arguments)]
pub async fn install(
    gh: &Github,
    entry: &Entry,
    i: &Installed,
    asset: &Asset,
    checksums: Option<&Asset>,
    version: &str,
    application: &str,
    out: &Messages,
) -> Result<(), String> {
    let crate::config::Kind::Executable(opts) = &entry.kind else { return Err("Not an executable entry".into()) };
    let prefix = entry.prefix();
    let name = i.asset_name.as_deref().ok_or("The installation has no asset_name")?;
    let staging = opts.path.join(format!(".{}.update", entry.name));
    let _ = tokio::fs::remove_dir_all(&staging).await;
    tokio::fs::create_dir(&staging).await.map_err(|e| format!("Can't make {}: {e}", staging.display()))?;
    let result = async {
        let download = staging.join(name);
        gh.download(asset, &download, application, out).await?;
        if let Some(sums_name) = &opts.checksums {
            let sums = checksums.ok_or_else(|| {
                format!("Release {version} has no {sums_name}, so orrery can't check the download.")
            })?;
            let sums_file = staging.join(format!(".{sums_name}"));
            gh.download(sums, &sums_file, application, out).await?;
            let text = tokio::fs::read_to_string(&sums_file).await.unwrap_or_default();
            let expected = expected_sum(&text, name)
                .ok_or_else(|| format!("The {sums_name} of {version} has no line for {name}, so orrery can't check it."))?;
            let actual = sha256(&download).await?;
            if actual != expected {
                return Err(format!(
                    "The download of {name} doesn't match its checksum, so nothing was changed.\nExpected {expected}, got {actual}."
                ));
            }
            let _ = out.send(format!("{prefix}The download matches its checksum."));
        }
        let _ = out.send(format!("{prefix}Download Complete, extracting release to '{}'", opts.path.display()));
        if !opts.programs.is_empty() {
            unpack(&download, &staging).await?;
        }
        let files = files(opts, i);
        for file in &files {
            let new = staging.join(file);
            if !new.is_file() {
                return Err(format!("{name} has no {file}, so nothing was changed."));
            }
            set_mode(&opts.path.join(file), &new).await.map_err(|e| format!("Can't put the new {file} in place: {e}"))?;
        }
        check(entry, &files, &staging, out).await?;
        keep_previous(&opts.path, &files).await?;
        for file in &files {
            tokio::fs::rename(staging.join(file), opts.path.join(file))
                .await
                .map_err(|e| format!("Can't put the new {file} in place: {e}"))?;
        }
        // An archive's release_info.json names its repository, which may be a fork's.
        let info = staging.join("release_info.json");
        if !opts.programs.is_empty() && info.is_file() {
            tokio::fs::rename(&info, opts.path.join("release_info.json"))
                .await
                .map_err(|e| format!("Can't write {}: {e}", opts.path.join("release_info.json").display()))?;
        }
        set_version(&opts.path.join("release_info.json"), version).await
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&staging).await;
    result
}

/// Gives `new` the permissions of the file it replaces, executable for its owner and group,
/// as Moonraker's `_set_exec_perms` makes a program.
async fn set_mode(old: &Path, new: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match tokio::fs::metadata(old).await {
        Ok(m) => m.permissions().mode(),
        Err(_) => tokio::fs::metadata(new).await?.permissions().mode(),
    };
    tokio::fs::set_permissions(new, std::fs::Permissions::from_mode(mode | 0o110)).await
}

/// The entry's preflight, run from the new or the old `files` in `dir`, before they take
/// effect, in the entry's folder. For one program, the words are its arguments. For an
/// archive's programs, the first word names the one to run; it finds the others beside its
/// own path.
async fn check(entry: &Entry, files: &[String], dir: &Path, out: &Messages) -> Result<(), String> {
    let crate::config::Kind::Executable(opts) = &entry.kind else { return Ok(()) };
    let Some(first) = entry.preflight.first() else { return Ok(()) };
    let _ = out.send(format!("{}Running the preflight check...", entry.prefix()));
    let (program, args) = if opts.programs.is_empty() {
        (files.first().map(String::as_str).unwrap_or_default(), &entry.preflight[..])
    } else {
        (first.as_str(), &entry.preflight[1..])
    };
    crate::preflight::run(program, &dir.join(program), args, &opts.path, crate::preflight::LIMIT).await
}

/// The checksum a `SHA256SUMS` file gives for `name`, in lowercase hex.
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        (file.trim_start().trim_start_matches('*') == name).then(|| sum.to_ascii_lowercase())
    })
}

async fn sha256(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.map_err(|e| format!("Can't read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpacks a `.tar.gz` with the system's `tar`, as the agent runs `git`.
async fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let out = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("--no-same-owner")
        .arg("-C")
        .arg(into)
        .output()
        .await
        .map_err(|e| format!("Can't run tar: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("Can't unpack {}: {}", archive.display(), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The folder that keeps the files and `release_info.json` an update replaced, for a
/// rollback without network access.
pub const PREVIOUS: &str = ".previous";

/// Hard-links `files` and `release_info.json` in `path` into `.previous/`, replacing what an
/// earlier update kept there. They're the same files until the update renames new ones over
/// them.
async fn keep_previous(path: &Path, files: &[String]) -> Result<(), String> {
    let previous = path.join(PREVIOUS);
    tokio::fs::create_dir_all(&previous).await.map_err(|e| format!("Can't make {}: {e}", previous.display()))?;
    for file in files.iter().map(String::as_str).chain(["release_info.json"]) {
        let (from, to) = (path.join(file), previous.join(file));
        let _ = tokio::fs::remove_file(&to).await;
        if from.exists() {
            tokio::fs::hard_link(&from, &to)
                .await
                .map_err(|e| format!("Can't keep the old {file} in {}: {e}", previous.display()))?;
        }
    }
    Ok(())
}

/// The version `.previous/` holds, when it holds each of `files` and their
/// `release_info.json`. A few small reads, so the status can show it.
pub fn previous_version(path: &Path, files: &[String]) -> Option<String> {
    let previous = path.join(PREVIOUS);
    if files.is_empty() || !files.iter().all(|f| previous.join(f).is_file()) {
        return None;
    }
    let text = std::fs::read_to_string(previous.join("release_info.json")).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    data["version"].as_str().map(str::to_owned)
}

/// Puts `files` and `release_info.json` from `.previous/` back, after the entry's preflight
/// passes for the old ones. `.previous/` is empty afterwards.
pub async fn restore_previous(entry: &Entry, files: &[String], out: &Messages) -> Result<(), String> {
    let crate::config::Kind::Executable(opts) = &entry.kind else { return Err("Not an executable entry".into()) };
    let previous = opts.path.join(PREVIOUS);
    check(entry, files, &previous, out).await?;
    for file in files.iter().map(String::as_str).chain(["release_info.json"]) {
        let from = previous.join(file);
        if from.exists() {
            tokio::fs::rename(&from, opts.path.join(file))
                .await
                .map_err(|e| format!("Can't put the old {file} back from {}: {e}", previous.display()))?;
        }
    }
    Ok(())
}

/// Writes `version` into `release_info.json`, when it says another one. The file is written
/// beside the old one and renamed over it.
async fn set_version(info: &Path, version: &str) -> Result<(), String> {
    let Ok(text) = tokio::fs::read_to_string(info).await else { return Ok(()) };
    let mut data: Map<String, Value> =
        serde_json::from_str(&text).map_err(|e| format!("Can't read {}: {e}", info.display()))?;
    if data.get("version").and_then(Value::as_str) == Some(version) {
        return Ok(());
    }
    data.insert("version".into(), json!(version));
    let partial = info.with_extension("json.new");
    let text = serde_json::to_string(&data).expect("JSON serializes");
    tokio::fs::write(&partial, text).await.map_err(|e| format!("Can't write {}: {e}", partial.display()))?;
    // The new file keeps the old one's permissions.
    if let Ok(meta) = tokio::fs::metadata(info).await {
        let _ = tokio::fs::set_permissions(&partial, meta.permissions()).await;
    }
    tokio::fs::rename(&partial, info).await.map_err(|e| format!("Can't write {}: {e}", info.display()))
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// `2026-11-22 15:04 UTC`, from seconds since 1970.
fn utc(t: f64) -> String {
    let secs = t as i64;
    let (days, rest) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}

/// TLS with a pure-Rust crypto provider and the system's trusted certificates, as the
/// Spoolman agent sets it up.
fn tls_config() -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls_rustcrypto::provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|e| format!("Can't load the system's certificates: {e}"))?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(config)
}

/// An error with its causes, which hold the useful part, such as "Connection refused".
fn error_chain(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::config;

    /// A fake GitHub: each path answers with its body, and with 304 when the request sends
    /// the `ETag` it gave. `routes` gets the server's address, for download URLs.
    pub(crate) async fn server(routes: impl FnOnce(&str) -> HashMap<String, Vec<u8>>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let routes = routes(&base);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let etag = format!("\"{}\"", path.len());
                    let head = |status: &str, len: usize| {
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {len}\r\nETag: {etag}\r\nX-RateLimit-Limit: 60\r\nX-RateLimit-Remaining: 59\r\nX-RateLimit-Reset: 1795000000\r\nConnection: close\r\n\r\n"
                        )
                    };
                    let reply = if req.to_ascii_lowercase().contains(&format!("if-none-match: {etag}")) {
                        head("304 Not Modified", 0).into_bytes()
                    } else if let Some(body) = routes.get(&path) {
                        [head("200 OK", body.len()).into_bytes(), body.clone()].concat()
                    } else {
                        head("404 Not Found", 0).into_bytes()
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });
        base
    }

    fn entry(dir: &Path, extra: Value) -> Entry {
        let g = config::global(&Map::new(), &mut Vec::new());
        let mut map =
            json!({"type": "executable", "path": dir, "repo": "ressu/printerkeep", "is_system_service": false});
        map.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        config::entry("printerkeep", map.as_object().unwrap(), &g).unwrap()
    }

    fn opts(e: &Entry) -> &ExecOptions {
        let config::Kind::Executable(o) = &e.kind else { panic!("an executable") };
        o
    }

    fn install_v1(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("printerkeep-linux-aarch64"), b"old").unwrap();
        let info = dir.join("release_info.json");
        std::fs::write(
            &info,
            r#"{"project_name":"printerkeep","project_owner":"ressu","version":"v1.1.0","asset_name":"printerkeep-linux-aarch64"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o664)).unwrap();
    }

    #[tokio::test]
    async fn reads_release_info() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry(dir.path(), json!({}));
        let i = installed(opts(&e)).await;
        assert!(!i.valid);
        assert_eq!(i.warnings, ["Failed to validate installation"]);
        install_v1(dir.path());
        let i = installed(opts(&e)).await;
        assert!(i.valid, "{i:?}");
        assert_eq!((i.version.as_str(), i.owner(), i.project()), ("v1.1.0", "ressu", "printerkeep"));
        let status = status(&e, &i, &Saved::default());
        assert_eq!(
            (status["rollback_version"].as_str(), status["remote_version"].as_str()),
            (Some("v1.1.0"), Some("?"))
        );

        // Another repo in the config: release_info.json wins, with an anomaly.
        let other = entry(dir.path(), json!({"repo": "someone/else"}));
        let i = installed(opts(&other)).await;
        assert_eq!(i.repo, "ressu/printerkeep");
        assert!(i.anomalies[0].contains("does not match detected repo 'ressu/printerkeep'"));
        // The program itself is missing.
        std::fs::remove_file(dir.path().join("printerkeep-linux-aarch64")).unwrap();
        let i = installed(opts(&e)).await;
        assert!(!i.valid && i.warnings[0].starts_with("File printerkeep-linux-aarch64 not found"));
        // Inside a git repo.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(installed(opts(&e)).await.warnings[0].contains("is within a git repo"));
    }

    fn release(tag: &str, base: &str) -> Vec<u8> {
        json!({"tag_name": tag, "prerelease": false, "assets": [
            {"name": "printerkeep-linux-x86_64", "browser_download_url": format!("{base}dl/x86"), "content_type": "application/octet-stream", "size": 3},
            {"name": "printerkeep-linux-aarch64", "browser_download_url": format!("{base}dl/{tag}"), "content_type": "application/octet-stream", "size": 6},
        ]})
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn checks_and_installs_releases() {
        let base = server(|base| {
            HashMap::from([
                ("/repos/ressu/printerkeep/releases/latest".to_owned(), release("v1.2.0", base)),
                ("/repos/ressu/printerkeep/releases/tags/v1.1.0".to_owned(), release("v1.1.0", base)),
                ("/dl/v1.2.0".to_owned(), b"new120".to_vec()),
                ("/dl/v1.1.0".to_owned(), b"old110".to_vec()),
            ])
        })
        .await;
        let gh = Github::with_api(&base).unwrap();

        let dir = tempfile::tempdir().unwrap();
        install_v1(dir.path());
        let e = entry(dir.path(), json!({}));
        let i = installed(opts(&e)).await;
        let mut saved = Saved::default();
        refresh(&gh, &e, &i, &mut saved).await;
        assert_eq!(saved.last_error, "");
        assert_eq!(saved.remote_version, "v1.2.0");
        assert_eq!(saved.asset.as_ref().unwrap().url, format!("{base}dl/v1.2.0"));
        assert_eq!(gh.rate(), Rate { limit: Some(60), remaining: Some(59), reset: Some(1_795_000_000.0) });
        // Asked again with the ETag: nothing changed, and the release stays as it was.
        assert!(saved.etag.is_some());
        refresh(&gh, &e, &i, &mut saved).await;
        assert_eq!(saved.remote_version, "v1.2.0");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        install(&gh, &e, &i, saved.asset.as_ref().unwrap(), None, "v1.2.0", "printerkeep", &tx).await.unwrap();
        drop(tx);
        let mut messages = Vec::new();
        while let Some(m) = rx.recv().await {
            messages.push(m);
        }
        // A small download shows no progress.
        assert!(
            messages[0].starts_with("Executable printerkeep: Download Complete, extracting release to"),
            "{messages:?}"
        );
        let program = dir.path().join("printerkeep-linux-aarch64");
        assert_eq!(std::fs::read(&program).unwrap(), b"new120");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&program).unwrap().permissions().mode() & 0o110, 0o110);
        assert!(!dir.path().join(".printerkeep.update").exists());
        let i = installed(opts(&e)).await;
        assert_eq!(i.version, "v1.2.0");
        let info = std::fs::read_to_string(dir.path().join("release_info.json")).unwrap();
        let info_mode = std::fs::metadata(dir.path().join("release_info.json")).unwrap().permissions().mode();
        assert_eq!(info_mode & 0o777, 0o664);
        assert!(
            info.starts_with(r#"{"project_name":"printerkeep","project_owner":"ressu","version":"v1.2.0""#),
            "{info}"
        );

        // A rollback finds the older release by its tag.
        let Fetched::Release(old, _) =
            gh.release("ressu/printerkeep", Channel::Stable, Some("v1.1.0"), None).await.unwrap()
        else {
            panic!("a release")
        };
        let (_, asset, _) = read_release(&old, i.asset_name.as_deref());
        assert_eq!(asset.unwrap().url, format!("{base}dl/v1.1.0"));

        // A download that's cut short leaves the program as it was.
        let short = Asset { size: 99, ..saved.asset.clone().unwrap() };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = install(&gh, &e, &i, &short, None, "v9", "printerkeep", &tx).await.unwrap_err();
        assert!(err.contains("has 6 bytes, not 99"), "{err}");
        assert_eq!(std::fs::read(&program).unwrap(), b"new120");
        assert!(!dir.path().join(".printerkeep.update").exists());
        // A missing release is an error, kept in last_error.
        let gone = entry(dir.path(), json!({"channel": "beta"}));
        refresh(&gh, &gone, &i, &mut saved).await;
        assert!(saved.last_error.starts_with("GitHub answered 404"), "{}", saved.last_error);
    }

    #[tokio::test]
    async fn keeps_the_old_program_and_checks_the_new_one() {
        const GOOD: &[u8] = b"#!/bin/sh\ntest \"$1\" = --check\n";
        const BAD: &[u8] = b"#!/bin/sh\necho broken config >&2\nexit 2\n";
        let base =
            server(|_| HashMap::from([("/dl/good".to_owned(), GOOD.to_vec()), ("/dl/bad".to_owned(), BAD.to_vec())]))
                .await;
        let gh = Github::with_api(&base).unwrap();
        let asset = |name: &str, body: &[u8]| Asset {
            url: format!("{base}dl/{name}"),
            content_type: "application/octet-stream".into(),
            size: body.len() as u64,
        };
        let dir = tempfile::tempdir().unwrap();
        install_v1(dir.path());
        let mut e = entry(dir.path(), json!({}));
        e.preflight = vec!["--check".into()];
        let i = installed(opts(&e)).await;
        let program = dir.path().join("printerkeep-linux-aarch64");
        let previous = dir.path().join(PREVIOUS);
        let files = vec!["printerkeep-linux-aarch64".to_owned()];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // A failed preflight leaves everything as it was.
        let err = install(&gh, &e, &i, &asset("bad", BAD), None, "v1.2.0", "printerkeep", &tx).await.unwrap_err();
        assert_eq!(
            err,
            "The preflight check `printerkeep-linux-aarch64 --check` failed with exit status 2:\nbroken config"
        );
        assert_eq!(std::fs::read(&program).unwrap(), b"old");
        assert!(!dir.path().join(".printerkeep.update").exists());
        assert!(!previous.exists());
        assert_eq!(installed(opts(&e)).await.version, "v1.1.0");

        // A passing one: the old program and release_info.json go to .previous/.
        install(&gh, &e, &i, &asset("good", GOOD), None, "v1.2.0", "printerkeep", &tx).await.unwrap();
        assert_eq!(std::fs::read(&program).unwrap(), GOOD);
        assert_eq!(std::fs::read(previous.join("printerkeep-linux-aarch64")).unwrap(), b"old");
        assert_eq!(previous_version(dir.path(), &files).as_deref(), Some("v1.1.0"));
        assert_eq!(installed(opts(&e)).await.version, "v1.2.0");

        // A rollback puts them back, without the network, after the old program's preflight.
        // The old program here is "old", which can't run, so the check fails first.
        let err = restore_previous(&e, &files, &tx).await.unwrap_err();
        assert!(err.starts_with("The preflight check `printerkeep-linux-aarch64 --check` can't start"), "{err}");
        assert_eq!(std::fs::read(&program).unwrap(), GOOD);
        e.preflight.clear();
        restore_previous(&e, &files, &tx).await.unwrap();
        assert_eq!(std::fs::read(&program).unwrap(), b"old");
        assert_eq!(installed(opts(&e)).await.version, "v1.1.0");
        assert_eq!(previous_version(dir.path(), &files), None);
    }

    /// A set of programs in `dir`, each a script that prints its name and `tag`. `tool` is
    /// the one with a preflight: `tool <args> check` passes when `tag` isn't "broken", the
    /// first argument is `-c`, and `tool-helper` is beside it.
    fn programs(dir: &Path, tag: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        for program in ["tool-helper", "tool"] {
            let body = if program == "tool" {
                format!(
                    "#!/bin/sh\nfor a; do last=$a; done\nif [ \"$last\" = check ]; then\n  [ {tag} != broken ] || {{ echo the config is broken >&2; exit 1; }}\n  [ \"$1\" = -c ] && [ -x \"$(dirname \"$0\")/tool-helper\" ] || exit 9\nfi\necho {program} {tag}\n"
                )
            } else {
                format!("#!/bin/sh\necho {program} {tag}\n")
            };
            let path = dir.join(program);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        }
    }

    /// A release archive of the programs, flat, with a release_info.json from `owner`.
    fn archive(tag: &str, owner: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("set");
        programs(&set, tag);
        let info = json!({"project_name": "tool", "project_owner": owner, "version": tag, "asset_name": "tool-aarch64.tar.gz"});
        std::fs::write(set.join("release_info.json"), info.to_string()).unwrap();
        let file = dir.path().join("a.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&file)
            .arg("-C")
            .arg(&set)
            .args(["tool-helper", "tool", "release_info.json"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(file).unwrap()
    }

    #[tokio::test]
    async fn archives_with_checksums() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;
        let (good, broken, fork) =
            (archive("v0.2.0", "example"), archive("broken", "example"), archive("v0.3.0", "someone"));
        let sum = |bytes: &[u8]| Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
        let base = server(|base| {
            let latest = json!({"tag_name": "v0.2.0", "assets": [
                {"name": "tool-aarch64.tar.gz", "browser_download_url": format!("{base}dl/good"), "size": good.len()},
                {"name": "tool-armv7.tar.gz", "browser_download_url": format!("{base}dl/other")},
                {"name": "SHA256SUMS", "browser_download_url": format!("{base}dl/sums")},
            ]});
            HashMap::from([
                ("/dl/good".to_owned(), good.clone()),
                ("/dl/broken".to_owned(), broken.clone()),
                ("/dl/fork".to_owned(), fork.clone()),
                ("/dl/sums".to_owned(), format!("{}  tool-aarch64.tar.gz\n", sum(&good)).into_bytes()),
                ("/dl/sums-broken".to_owned(), format!("{}  tool-aarch64.tar.gz\n", sum(&broken)).into_bytes()),
                ("/dl/sums-fork".to_owned(), format!("{} *tool-aarch64.tar.gz\n", sum(&fork)).into_bytes()),
                ("/dl/sums-other".to_owned(), format!("{}  tool-armv7.tar.gz\n", sum(&good)).into_bytes()),
                ("/repos/example/tool/releases/latest".to_owned(), latest.to_string().into_bytes()),
            ])
        })
        .await;
        let gh = Github::with_api(&base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        programs(dir.path(), "v0.1.0");
        let info = r#"{"project_name":"tool","project_owner":"example","version":"v0.1.0","asset_name":"tool-aarch64.tar.gz"}"#;
        std::fs::write(dir.path().join("release_info.json"), info).unwrap();
        let g = config::global(&Map::new(), &mut Vec::new());
        let map = json!({"type": "executable", "path": dir.path(), "repo": "example/tool",
            "checksums": "SHA256SUMS", "programs": ["tool-helper", "tool"], "preflight": ["tool", "-c", "x", "check"]});
        let map = map.as_object().unwrap();
        let mut e = config::entry("tool", map, &g).unwrap();
        let from_toml = ["checksums", "programs", "preflight"].map(str::to_owned);
        config::own_options("update_manager tool", map, &from_toml, &mut e, &mut Vec::new()).unwrap();
        let i = installed(opts(&e)).await;
        assert!(i.valid, "{:?}", i.warnings);
        let files = files(opts(&e), &i);
        assert_eq!(files, ["tool-helper", "tool"]);
        let read =
            |d: &Path| files.iter().map(|f| std::fs::read_to_string(d.join(f)).unwrap_or_default()).collect::<Vec<_>>();
        let before = read(dir.path());
        let unchanged = |what: &str| {
            assert_eq!(read(dir.path()), before, "{what}");
            assert!(!dir.path().join(".tool.update").exists(), "{what}");
            assert!(!dir.path().join(PREVIOUS).exists(), "{what}");
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let at = |path: &str| Asset { url: format!("{base}dl/{path}"), content_type: "?".into(), size: 0 };

        // The check finds the archive for this CPU, and the checksums.
        let mut saved = Saved::default();
        refresh(&gh, &e, &i, &mut saved).await;
        assert_eq!(saved.remote_version, "v0.2.0");
        assert!(saved.asset.as_ref().unwrap().url.ends_with("/dl/good"));
        assert!(saved.checksums.as_ref().unwrap().url.ends_with("/dl/sums"));

        // A download that doesn't match its checksum changes nothing, nor one that
        // SHA256SUMS doesn't list, nor a release without SHA256SUMS.
        let err = install(&gh, &e, &i, &at("broken"), Some(&at("sums")), "v0.2.0", "tool", &tx).await.unwrap_err();
        assert!(err.starts_with("The download of tool-aarch64.tar.gz doesn't match its checksum"), "{err}");
        unchanged("checksum");
        let err = install(&gh, &e, &i, &at("good"), Some(&at("sums-other")), "v0.2.0", "tool", &tx).await.unwrap_err();
        assert_eq!(err, "The SHA256SUMS of v0.2.0 has no line for tool-aarch64.tar.gz, so orrery can't check it.");
        unchanged("no line");
        let err = install(&gh, &e, &i, &at("good"), None, "v0.2.0", "tool", &tx).await.unwrap_err();
        assert_eq!(err, "Release v0.2.0 has no SHA256SUMS, so orrery can't check the download.");
        unchanged("no checksums");
        // Nor a failed preflight, which runs the new tool beside the new helper.
        let err =
            install(&gh, &e, &i, &at("broken"), Some(&at("sums-broken")), "v0.4.0", "tool", &tx).await.unwrap_err();
        assert_eq!(err, "The preflight check `tool -c x check` failed with exit status 1:\nthe config is broken");
        unchanged("preflight");

        // An update: the new set in place with the old modes, the old one in .previous/.
        install(&gh, &e, &i, saved.asset.as_ref().unwrap(), saved.checksums.as_ref(), "v0.2.0", "tool", &tx)
            .await
            .unwrap();
        assert!(read(dir.path()).iter().all(|c| c.ends_with(" v0.2.0\n")));
        assert_eq!(read(&dir.path().join(PREVIOUS)), before);
        let mode = std::fs::metadata(dir.path().join("tool")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
        assert_eq!(previous_version(dir.path(), &files).as_deref(), Some("v0.1.0"));
        assert_eq!(installed(opts(&e)).await.version, "v0.2.0");
        assert!(!dir.path().join(".tool.update").exists());
        // The status offers .previous/ for a rollback.
        assert_eq!(status(&e, &installed(opts(&e)).await, &saved)["rollback_version"], "v0.1.0");

        // A rollback puts the old set back, after its preflight.
        restore_previous(&e, &files, &tx).await.unwrap();
        assert_eq!(read(dir.path()), before);
        assert_eq!(installed(opts(&e)).await.version, "v0.1.0");
        assert_eq!(previous_version(dir.path(), &files), None);

        // A fork's archive brings its release_info.json, so its releases take over.
        let i = installed(opts(&e)).await;
        install(&gh, &e, &i, &at("fork"), Some(&at("sums-fork")), "v0.3.0", "tool", &tx).await.unwrap();
        assert_eq!(installed(opts(&e)).await.repo, "someone/tool");
        // Unless orrery.toml names the repository, which then wins without an anomaly.
        let from_toml = ["checksums", "programs", "preflight", "repo"].map(str::to_owned);
        config::own_options("update_manager tool", map, &from_toml, &mut e, &mut Vec::new()).unwrap();
        let i = installed(opts(&e)).await;
        assert_eq!((i.repo.as_str(), i.anomalies.len()), ("example/tool", 0));
    }

    #[test]
    fn dev_channel_falls_back_to_stable() {
        let e = entry(Path::new("/x"), json!({"channel": "dev"}));
        assert_eq!(e.channel, Channel::Stable);
        assert!(e.channel_warning.unwrap().contains("Falling back to channel 'stable'"));
    }

    #[test]
    fn utc_times() {
        assert_eq!(utc(0.0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_795_000_000.0), "2026-11-18 11:06 UTC");
    }
}
