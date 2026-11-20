//! Files in the `config`, `logs`, and `gcodes` roots, after Moonraker's `file_manager.py`, and
//! the `server.files.*` endpoints.
//!
//! orrery sees the changes made through its own API, and [`watch`] sees the ones made to the
//! folders directly, with scp or by Klipper. It scans the gcodes folder for metadata when it
//! starts.

pub mod http;
mod metadata;
mod watch;

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler};
use crate::config::Config;
use crate::database::Database;
use crate::klippy::KlippyState;
use crate::server::{App, Paths};

pub const VALID_GCODE_EXTS: &[&str] = &[".gcode", ".g", ".gco", ".ufp", ".nc"];
/// The store's internal namespace for gcode metadata. Clients can't reach it.
const METADATA_NAMESPACE: &str = "gcode_metadata";

pub struct Root {
    pub name: String,
    pub path: PathBuf,
    pub full_access: bool,
}

pub struct FileManager {
    roots: Vec<Root>,
    /// Paths clients can't write, and can't read either when the flag is false.
    reserved: Vec<(String, PathBuf, bool)>,
    /// Gcode metadata by path in the gcodes root. A copy of what the store holds.
    metadata: Mutex<BTreeMap<String, Map<String, Value>>>,
    /// Held during each file operation, as Moonraker's `sync_lock` is.
    op_lock: tokio::sync::Mutex<()>,
    /// Metadata is parsed one file at a time.
    parse_lock: tokio::sync::Mutex<()>,
    pub max_upload_size: usize,
    /// Uploads that ask to print go into the job queue when the print can't start.
    pub queue_gcodes: bool,
    /// `file_system_observer`: `inotify` watches the roots, `none` doesn't.
    observer: String,
    observer_warnings: bool,
    /// Paths the API is changing, for the watcher to leave out.
    own_changes: Mutex<Vec<watch::OwnChange>>,
    next_own_change: AtomicU64,
}

/// The file extension, lowercase and with its dot, as Python's `os.path.splitext` gives it.
pub fn extension(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.trim_start_matches('.').rfind('.') {
        Some(i) => base[i + (base.len() - base.trim_start_matches('.').len())..].to_lowercase(),
        None => String::new(),
    }
}

pub fn is_gcode(name: &str) -> bool {
    VALID_GCODE_EXTS.contains(&extension(name).as_str())
}

/// Collapses `.` and `..` without touching the disk, as Python's `os.path.normpath` does.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Resolves symlinks in the part of `path` that exists, as `pathlib.Path.resolve()` does.
fn resolve(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut existing = path.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return rest.iter().rev().fold(real, |p: PathBuf, c| p.join(c));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path,
        }
    }
}

fn mtime(meta: &std::fs::Metadata) -> f64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or_default()
}

fn io_error(e: io::Error) -> ApiError {
    ApiError::bad_request(e.to_string())
}

impl FileManager {
    pub fn new(config: &Config, paths: &Paths) -> Result<Self> {
        let section = config.section("file_manager");
        let config_write = section.get_bool("enable_config_write_access", true)?;
        let queue_gcodes = section.get_bool("queue_gcode_uploads", false)?;
        section.get_bool("check_klipper_config_path", true)?;
        let observer = section.get_str("file_system_observer", "inotify").to_lowercase();
        let observer_warnings = section.get_bool("enable_observer_warnings", true)?;
        drop(section);
        let max_upload_size = config.section("server").get_int("max_upload_size", 1024)?.max(1) as usize * 1024 * 1024;
        let mut roots = Vec::new();
        for (name, full_access) in [("config", config_write), ("logs", false), ("gcodes", true)] {
            let path = paths.data.join(name);
            if !path.exists() {
                let _ = std::fs::create_dir(&path);
            }
            let path = path.canonicalize().unwrap_or(path);
            roots.push(Root { name: name.into(), path, full_access });
        }
        let reserved = [
            ("database", paths.database_dir(), false),
            ("certs", paths.data.join("certs"), false),
            ("systemd", paths.data.join("systemd"), false),
            ("backup", paths.data.join("backup"), false),
            ("secrets", paths.data.join("moonraker.secrets"), false),
            ("allowed_services", paths.data.join("moonraker.asvc"), false),
        ]
        .into_iter()
        .map(|(n, p, r)| (n.to_owned(), resolve(&p), r))
        .collect();
        Ok(Self {
            roots,
            reserved,
            metadata: Mutex::new(BTreeMap::new()),
            op_lock: tokio::sync::Mutex::new(()),
            parse_lock: tokio::sync::Mutex::new(()),
            max_upload_size,
            queue_gcodes,
            observer,
            observer_warnings,
            own_changes: Mutex::new(Vec::new()),
            next_own_change: AtomicU64::new(0),
        })
    }

    pub fn root_names(&self) -> Vec<&str> {
        self.roots.iter().map(|r| r.name.as_str()).collect()
    }

    pub fn root(&self, name: &str) -> Option<&Root> {
        self.roots.iter().find(|r| r.name == name)
    }

    fn root_path(&self, name: &str) -> Result<&Path, ApiError> {
        self.root(name).map(|r| r.path.as_path()).ok_or_else(|| ApiError::bad_request(format!("Unknown root {name}")))
    }

    pub fn full_access(&self, name: &str) -> bool {
        self.root(name).is_some_and(|r| r.full_access)
    }

    pub fn gcodes_dir(&self) -> &Path {
        &self.root("gcodes").expect("gcodes is always registered").path
    }

    /// Splits a request path like `gcodes/sub/a.gcode` into its root and the path on disk.
    pub fn convert_request_path(&self, request: &str) -> Result<(String, PathBuf), ApiError> {
        let norm = normalize(Path::new(request));
        let norm = norm.to_string_lossy();
        let mut parts = norm.trim_matches('/').splitn(2, '/');
        let root = parts.next().unwrap_or_default().to_owned();
        let root_path = self.root(&root).ok_or_else(|| ApiError::bad_request(format!("Invalid root path ({root})")))?;
        let mut dest = root_path.path.clone();
        if let Some(rest) = parts.next() {
            dest = normalize(&dest.join(rest));
            if !dest.starts_with(&root_path.path) {
                return Err(ApiError::bad_request(format!(
                    "Invalid path request, '{request}'' is outside root '{root}'"
                )));
            }
        }
        Ok((root, dest))
    }

    /// The path relative to a root, or an empty string if it isn't inside the root.
    pub fn relative_path(&self, root: &str, full: &Path) -> String {
        let Some(r) = self.root(root) else { return String::new() };
        match full.strip_prefix(&r.path) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => String::new(),
        }
    }

    /// Whether `path` is inside a reserved path that forbids this access.
    fn is_reserved(&self, path: &Path, need_write: bool) -> Option<String> {
        let real = resolve(path);
        if real.components().any(|c| c.as_os_str() == ".git") {
            return Some("Access to .git folders is forbidden".into());
        }
        for (name, res, can_read) in &self.reserved {
            if real.starts_with(res) && (need_write || !can_read) {
                let file = real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return Some(format!("Access to file {file} forbidden by reserved path '{name}'"));
            }
        }
        None
    }

    pub fn check_reserved(&self, path: &Path, need_write: bool) -> Result<(), ApiError> {
        match self.is_reserved(path, need_write) {
            Some(msg) => Err(ApiError::new(403, msg)),
            None => Ok(()),
        }
    }

    /// `modified`, `size`, and `permissions` for a file or folder.
    pub fn path_info(&self, path: &Path, root: &str) -> io::Result<Map<String, Value>> {
        let meta = std::fs::metadata(path)?;
        let real = resolve(path);
        let permissions = if real.components().any(|c| c.as_os_str() == ".git") {
            ""
        } else {
            let is_link_file = path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && meta.is_file();
            let mut p = if !self.full_access(root) || is_link_file { "r" } else { "rw" };
            for (_, res, can_read) in &self.reserved {
                if real.starts_with(res) {
                    if !can_read {
                        p = "";
                        break;
                    }
                    p = "r";
                }
            }
            p
        };
        let mut info = Map::new();
        info.insert("modified".into(), json!(mtime(&meta)));
        info.insert("size".into(), json!(meta.len()));
        info.insert("permissions".into(), json!(permissions));
        Ok(info)
    }

    fn path_info_or_empty(&self, path: &Path, root: &str) -> Map<String, Value> {
        self.path_info(path, root).unwrap_or_else(|_| {
            let mut m = Map::new();
            m.insert("modified".into(), json!(0));
            m.insert("size".into(), json!(0));
            m.insert("permissions".into(), json!(""));
            m
        })
    }

    /// Refuses to change a file that's printing, or a folder that holds it. Returns whether
    /// no print is running.
    pub fn operation_check(&self, app: &App, path: &Path) -> Result<bool, ApiError> {
        if self.relative_path("gcodes", path).is_empty() {
            return Ok(true);
        }
        let stats = app.job_state.last_stats();
        let loaded = stats.get("filename").and_then(Value::as_str).unwrap_or_default();
        let state = stats.get("state").and_then(Value::as_str).unwrap_or_default();
        let printing = app.klippy.state().0 == KlippyState::Ready && matches!(state, "printing" | "paused");
        if !loaded.is_empty() && printing {
            let full = self.gcodes_dir().join(loaded);
            let in_use = if path.is_dir() { full.starts_with(path) } else { full == path };
            if in_use {
                return Err(ApiError::new(403, "File currently in use"));
            }
        }
        Ok(!printing)
    }

    // *** Metadata ***

    pub fn get_metadata(&self, rel: &str) -> Option<Map<String, Value>> {
        self.metadata.lock().expect("metadata lock").get(rel).cloned()
    }

    /// Records the latest job for a file, as Moonraker's history does with `print_start_time`
    /// and `job_id`. Files without metadata are left alone.
    pub fn set_job_info(&self, db: &Database, rel: &str, start_time: f64, job_id: &str) {
        let Some(mut md) = self.get_metadata(rel) else { return };
        md.insert("print_start_time".into(), json!(start_time));
        md.insert("job_id".into(), json!(job_id));
        self.store_metadata(db, rel, md);
    }

    fn store_metadata(&self, db: &Database, rel: &str, md: Map<String, Value>) {
        if let Err(e) = db.local_put(METADATA_NAMESPACE, rel, &Value::Object(md.clone())) {
            tracing::warn!("Can't store metadata for {rel}: {e:#}");
        }
        self.metadata.lock().expect("metadata lock").insert(rel.to_owned(), md);
    }

    fn forget_metadata(&self, db: &Database, rels: &[String]) {
        if let Err(e) = db.local_delete(METADATA_NAMESPACE, rels) {
            tracing::warn!("Can't remove metadata: {e:#}");
        }
    }

    /// Whether the stored metadata still matches the file's size and modified time.
    fn has_valid_metadata(&self, rel: &str, info: &Map<String, Value>) -> bool {
        self.metadata
            .lock()
            .expect("metadata lock")
            .get(rel)
            .is_some_and(|md| md.get("size") == info.get("size") && md.get("modified") == info.get("modified"))
    }

    fn remove_thumbs(&self, records: &[(String, Map<String, Value>)]) {
        for (rel, md) in records {
            let dir = self.gcodes_dir().join(rel);
            let dir = dir.parent().unwrap_or(self.gcodes_dir());
            for thumb in md.get("thumbnails").and_then(Value::as_array).into_iter().flatten() {
                if let Some(p) = thumb.get("relative_path").and_then(Value::as_str) {
                    let _ = std::fs::remove_file(dir.join(p));
                }
            }
        }
    }

    fn remove_file_metadata(&self, db: &Database, rel: &str) {
        let removed = self.metadata.lock().expect("metadata lock").remove(rel);
        if let Some(md) = removed {
            self.forget_metadata(db, &[rel.to_owned()]);
            self.remove_thumbs(&[(rel.to_owned(), md)]);
        }
    }

    fn remove_directory_metadata(&self, db: &Database, dir: &str) {
        let prefix = if dir.ends_with('/') { dir.to_owned() } else { format!("{dir}/") };
        let removed: Vec<(String, Map<String, Value>)> = {
            let mut md = self.metadata.lock().expect("metadata lock");
            let keys: Vec<String> = md.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.into_iter().filter_map(|k| md.remove(&k).map(|v| (k, v))).collect()
        };
        if !removed.is_empty() {
            let keys: Vec<String> = removed.iter().map(|(k, _)| k.clone()).collect();
            self.forget_metadata(db, &keys);
            self.remove_thumbs(&removed);
        }
    }

    fn move_directory_metadata(&self, db: &Database, prev: &str, new: &str) {
        let prefix = if prev.ends_with('/') { prev.to_owned() } else { format!("{prev}/") };
        let moved: Vec<(String, String, Map<String, Value>)> = {
            let mut md = self.metadata.lock().expect("metadata lock");
            let keys: Vec<String> = md.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.into_iter()
                .filter_map(|k| {
                    let v = md.remove(&k)?;
                    let n = Path::new(new).join(&k[prefix.len()..]).to_string_lossy().into_owned();
                    md.insert(n.clone(), v.clone());
                    Some((k, n, v))
                })
                .collect()
        };
        if !moved.is_empty() {
            self.forget_metadata(db, &moved.iter().map(|m| m.0.clone()).collect::<Vec<_>>());
            for (_, n, v) in moved {
                let _ = db.local_put(METADATA_NAMESPACE, &n, &Value::Object(v));
            }
        }
    }

    /// Moves a file's metadata and thumbnails. Returns false if it had none, so the new file
    /// needs a scan.
    fn move_file_metadata(&self, db: &Database, prev: &str, new: &str) -> bool {
        let md = self.metadata.lock().expect("metadata lock").remove(prev);
        let Some(md) = md else {
            // A move over an existing file needs a rescan, so its old metadata goes.
            if self.metadata.lock().expect("metadata lock").remove(new).is_some() {
                self.forget_metadata(db, &[new.to_owned()]);
            }
            return false;
        };
        self.forget_metadata(db, &[prev.to_owned()]);
        let gc = self.gcodes_dir();
        let prev_dir = gc.join(prev).parent().map(Path::to_path_buf).unwrap_or_else(|| gc.to_path_buf());
        let new_dir = gc.join(new).parent().map(Path::to_path_buf).unwrap_or_else(|| gc.to_path_buf());
        for thumb in md.get("thumbnails").and_then(Value::as_array).into_iter().flatten() {
            let Some(p) = thumb.get("relative_path").and_then(Value::as_str) else { continue };
            let from = prev_dir.join(p);
            if !from.is_file() {
                continue;
            }
            let to = new_dir.join(p);
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = move_path(&from, &to) {
                tracing::info!("Error moving thumb from {} to {}: {e}", from.display(), to.display());
            }
        }
        self.store_metadata(db, new, md);
        true
    }
}

/// Moves a file or folder, copying when it has to cross file systems.
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
            if from.is_dir() {
                copy_tree(from, to)?;
                std::fs::remove_dir_all(from)
            } else {
                copy_file(from, to)?;
                std::fs::remove_file(from)
            }
        }
        other => other,
    }
}

/// Copies a file with its modified time, as Python's `shutil.copy2` does.
fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::copy(from, to)?;
    let modified = std::fs::metadata(from)?.modified()?;
    std::fs::File::options().write(true).open(to)?.set_modified(modified)
}

/// Copies a folder into a new one, as Python's `shutil.copytree` does.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            copy_file(&entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Loads the stored metadata and scans the gcodes folder for files that changed while orrery
/// was stopped.
pub fn start(app: &Arc<App>) {
    let fm = &app.files;
    match app.database.local_items(METADATA_NAMESPACE) {
        Ok(items) => {
            let mut md = fm.metadata.lock().expect("metadata lock");
            for (k, v) in items {
                if let Value::Object(m) = v {
                    md.insert(k, m);
                }
            }
        }
        Err(e) => tracing::warn!("Can't load gcode metadata: {e:#}"),
    }
    // Prune metadata for files removed while orrery was stopped.
    let gone: Vec<String> = {
        let md = fm.metadata.lock().expect("metadata lock");
        md.keys().filter(|k| !fm.gcodes_dir().join(k).is_file()).cloned().collect()
    };
    if !gone.is_empty() {
        tracing::info!("Pruned metadata for the following:\n{}", gone.join("\n"));
        for k in &gone {
            fm.remove_file_metadata(&app.database, k);
        }
    }
    let app_for_watch = app.clone();
    let app = app.clone();
    tokio::spawn(async move {
        let dir = app.files.gcodes_dir().to_path_buf();
        let started = std::time::Instant::now();
        let count = scan_directory_metadata(&app, &dir).await;
        tracing::info!("Checked metadata for {count} gcode files in {:.1?}", started.elapsed());
    });
    watch::start(&app_for_watch);
}

/// Parses a gcode file's metadata unless the stored copy is still current.
pub async fn parse_metadata(app: &Arc<App>, full: &Path) {
    let fm = &app.files;
    let rel = fm.relative_path("gcodes", full);
    let Ok(info) = fm.path_info(full, "gcodes") else { return };
    if !is_gcode(&rel) || info.get("size").and_then(Value::as_u64) == Some(0) {
        return;
    }
    let _guard = fm.parse_lock.lock().await;
    if fm.has_valid_metadata(&rel, &info) {
        return;
    }
    // The thumbnails go into `.thumbs` next to the file. Moonraker announces them too, but
    // clients hide that folder.
    let thumbs = full.parent().map(|p| p.join(".thumbs"));
    let _own = thumbs.as_deref().map(|t| fm.own_change(&[t]));
    let path = full.to_path_buf();
    let mut result = Err(anyhow::anyhow!("not run"));
    for _ in 0..3 {
        let p = path.clone();
        result = tokio::task::spawn_blocking(move || metadata::extract(&p))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("metadata task failed: {e}")));
        if result.is_ok() {
            break;
        }
    }
    let mut md = match result {
        Ok(md) => md,
        Err(e) => {
            tracing::info!("Unable to extract metadata from file: {rel}: {e:#}");
            let mut md = Map::new();
            md.insert("size".into(), info.get("size").cloned().unwrap_or(json!(0)));
            md.insert("modified".into(), info.get("modified").cloned().unwrap_or(json!(0)));
            md
        }
    };
    md.insert("print_start_time".into(), Value::Null);
    md.insert("job_id".into(), Value::Null);
    fm.store_metadata(&app.database, &rel, md);
}

/// Parses every gcode file below `start`. Returns how many it found.
async fn scan_directory_metadata(app: &Arc<App>, start: &Path) -> usize {
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(meta) = std::fs::metadata(&dir) else { continue };
        if !visited.insert((meta.dev(), meta.ino())) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file()
                && path
                    .extension()
                    .is_some_and(|e| VALID_GCODE_EXTS.contains(&format!(".{}", e.to_string_lossy()).as_str()))
            {
                files.push(path);
            }
        }
    }
    for f in &files {
        parse_metadata(app, f).await;
    }
    files.len()
}

/// Builds the `notify_filelist_changed` payload and sends it shortly after the reply.
pub fn changed_event(
    app: &Arc<App>,
    action: &str,
    root: &str,
    full: &Path,
    source: Option<(&str, &Path)>,
) -> Map<String, Value> {
    let fm = &app.files;
    let mut item = fm.path_info_or_empty(full, root);
    item.insert("path".into(), json!(fm.relative_path(root, full)));
    item.insert("root".into(), json!(root));
    let mut info = Map::new();
    info.insert("action".into(), json!(action));
    info.insert("item".into(), Value::Object(item));
    if let Some((src_root, src_path)) = source {
        info.insert("source_item".into(), json!({"path": fm.relative_path(src_root, src_path), "root": src_root}));
    }
    let app = app.clone();
    let payload = Value::Object(info.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        app.notify("filelist_changed", vec![payload]);
    });
    info
}

/// Updates metadata after a move, after Moonraker's `try_move_metadata`.
async fn after_move(app: &Arc<App>, src_root: &str, dest_root: &str, src: &Path, dest: &Path) {
    let fm = &app.files;
    let is_dir = dest.is_dir();
    let needs_scan = if dest_root == "gcodes" {
        if src_root == "gcodes" {
            let prev = fm.relative_path("gcodes", src);
            let new = fm.relative_path("gcodes", dest);
            if is_dir {
                fm.move_directory_metadata(&app.database, &prev, &new);
                false
            } else {
                !fm.move_file_metadata(&app.database, &prev, &new)
            }
        } else {
            true
        }
    } else {
        if src_root == "gcodes" {
            clear_metadata(app, src_root, src, is_dir);
        }
        false
    };
    if needs_scan {
        if is_dir {
            scan_directory_metadata(app, dest).await;
        } else if dest.is_file() && is_gcode(&dest.to_string_lossy()) {
            parse_metadata(app, dest).await;
        }
    }
}

async fn after_copy(app: &Arc<App>, root: &str, path: &Path) {
    if root != "gcodes" {
        return;
    }
    if path.is_file() && is_gcode(&path.to_string_lossy()) {
        parse_metadata(app, path).await;
    } else if path.is_dir() {
        scan_directory_metadata(app, path).await;
    }
}

fn clear_metadata(app: &Arc<App>, root: &str, path: &Path, is_dir: bool) {
    if root == "gcodes" {
        let rel = app.files.relative_path(root, path);
        if is_dir {
            app.files.remove_directory_metadata(&app.database, &rel);
        } else {
            app.files.remove_file_metadata(&app.database, &rel);
        }
    }
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register("/server/files/list", &[RequestType::Get], Transports::All, handler(file_list));
    r.register("/server/files/metadata", &[RequestType::Get], Transports::All, handler(file_metadata));
    r.register("/server/files/metascan", &[RequestType::Post], Transports::All, handler(metascan));
    r.register("/server/files/thumbnails", &[RequestType::Get], Transports::All, handler(thumbnails));
    r.register("/server/files/roots", &[RequestType::Get], Transports::All, handler(list_roots));
    r.register(
        "/server/files/directory",
        &[RequestType::Get, RequestType::Post, RequestType::Delete],
        Transports::All,
        handler(directory),
    );
    r.register("/server/files/move", &[RequestType::Post], Transports::All, handler(move_copy));
    r.register("/server/files/copy", &[RequestType::Post], Transports::All, handler(move_copy));
    r.register("/server/files/zip", &[RequestType::Post], Transports::All, handler(zip_files));
    r.register("/server/files/delete_file", &[RequestType::Delete], Transports::WebsocketOnly, handler(delete_request));
}

async fn file_list(app: Arc<App>, req: Request) -> ApiResult {
    let root = req.args.opt_str("root")?.unwrap_or_else(|| "gcodes".into());
    let fm = &app.files;
    let Some(r) = fm.root(&root).filter(|r| r.path.is_dir()) else {
        return Err(ApiError::bad_request(format!("Failed to build file list, invalid path: {root}: None")));
    };
    let base = r.path.clone();
    let mut files: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut visited = HashSet::new();
    if let Ok(m) = std::fs::metadata(&base) {
        visited.insert((m.dev(), m.ino()));
    }
    let mut stack = vec![base.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_dir() {
                if visited.insert((meta.dev(), meta.ino())) && fm.is_reserved(&path, false).is_none() {
                    stack.push(path);
                }
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if root == "gcodes" && !is_gcode(&name) {
                continue;
            }
            let rel = path.strip_prefix(&base).map(|p| p.to_string_lossy().into_owned()).unwrap_or(name);
            if let Ok(info) = fm.path_info(&path, &root) {
                files.push((rel, info));
            }
        }
    }
    files.sort_by_key(|(name, _)| name.to_lowercase());
    let list: Vec<Value> = files
        .into_iter()
        .map(|(path, info)| {
            let mut m = Map::new();
            m.insert("path".into(), json!(path));
            m.extend(info);
            Value::Object(m)
        })
        .collect();
    Ok(Value::Array(list))
}

async fn file_metadata(app: Arc<App>, req: Request) -> ApiResult {
    let filename = req.args.str("filename")?;
    // The startup scan can take minutes on a large folder, so a file it hasn't reached yet is
    // parsed now. Moonraker answers 404 until its scan gets there.
    if app.files.get_metadata(&filename).is_none() {
        let path = normalize(&app.files.gcodes_dir().join(&filename));
        if path.starts_with(app.files.gcodes_dir()) && path.is_file() {
            parse_metadata(&app, &path).await;
        }
    }
    let mut md = app
        .files
        .get_metadata(&filename)
        .ok_or_else(|| ApiError::not_found(format!("Metadata not available for <{filename}>")))?;
    md.insert("filename".into(), json!(filename));
    Ok(Value::Object(md))
}

async fn metascan(app: Arc<App>, req: Request) -> ApiResult {
    let _guard = app.files.op_lock.lock().await;
    let filename = req.args.str("filename")?;
    let path = app.files.gcodes_dir().join(&filename);
    if !path.is_file() {
        return Err(ApiError::not_found(format!("File '{filename}' does not exist")));
    }
    if !path.extension().is_some_and(|e| VALID_GCODE_EXTS.contains(&format!(".{}", e.to_string_lossy()).as_str())) {
        return Err(ApiError::bad_request(format!("File {} is not a valid gcode file", path.display())));
    }
    app.files.remove_file_metadata(&app.database, &filename);
    parse_metadata(&app, &path).await;
    let mut md = app
        .files
        .get_metadata(&filename)
        .ok_or_else(|| ApiError::new(500, format!("Failed to parse metadata for file '{filename}'")))?;
    md.insert("filename".into(), json!(filename));
    Ok(Value::Object(md))
}

async fn thumbnails(app: Arc<App>, req: Request) -> ApiResult {
    let filename = req.args.str("filename")?;
    let Some(md) = app.files.get_metadata(&filename) else { return Ok(json!([])) };
    let Some(Value::Array(thumbs)) = md.get("thumbnails") else { return Ok(json!([])) };
    let parent = Path::new(&filename).parent().unwrap_or(Path::new(""));
    let list: Vec<Value> = thumbs
        .iter()
        .map(|t| {
            let mut t = t.as_object().cloned().unwrap_or_default();
            if let Some(Value::String(rel)) = t.remove("relative_path") {
                t.insert("thumbnail_path".into(), json!(parent.join(rel).to_string_lossy()));
            }
            Value::Object(t)
        })
        .collect();
    Ok(Value::Array(list))
}

async fn list_roots(app: Arc<App>, _req: Request) -> ApiResult {
    let roots: Vec<Value> = app
        .files
        .roots
        .iter()
        .map(|r| {
            json!({"name": r.name, "path": r.path.to_string_lossy(), "permissions": if r.full_access { "rw" } else { "r" }})
        })
        .collect();
    Ok(Value::Array(roots))
}

fn disk_usage(path: &Path) -> Value {
    match rustix::fs::statvfs(path) {
        Ok(s) => json!({
            "total": s.f_blocks * s.f_frsize,
            "used": (s.f_blocks - s.f_bfree) * s.f_frsize,
            "free": s.f_bavail * s.f_frsize,
        }),
        Err(_) => json!({"total": 0, "used": 0, "free": 0}),
    }
}

fn list_directory(app: &App, path: &Path, root: &str, extended: bool) -> ApiResult {
    let fm = &app.files;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!("Directory does not exist ({})", path.display())));
    }
    fm.check_reserved(path, false)?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).map_err(io_error)?.flatten() {
        let full = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(mut info) = fm.path_info(&full, root) else { continue };
        if full.is_dir() {
            info.insert("dirname".into(), json!(name));
            dirs.push(Value::Object(info));
        } else if full.is_file() {
            info.insert("filename".into(), json!(name));
            if root == "gcodes"
                && is_gcode(&name)
                && extended
                && let Some(md) = fm.get_metadata(&fm.relative_path(root, &full))
            {
                info.extend(md);
            }
            files.push(Value::Object(info));
        }
    }
    Ok(json!({
        "dirs": dirs,
        "files": files,
        "disk_usage": disk_usage(path),
        "root_info": {"name": root, "permissions": if fm.full_access(root) { "rw" } else { "r" }},
    }))
}

async fn directory(app: Arc<App>, req: Request) -> ApiResult {
    let directory = req.args.opt_str("path")?.unwrap_or_else(|| "gcodes".into());
    let (root, dir_path) = app.files.convert_request_path(&directory)?;
    if req.kind == RequestType::Get {
        let extended = req.args.opt_bool("extended")?.unwrap_or(false);
        return list_directory(&app, &dir_path, &root, extended);
    }
    let fm = &app.files;
    let _guard = fm.op_lock.lock().await;
    fm.check_reserved(&dir_path, true)?;
    let _own = fm.own_change(&[&dir_path]);
    let action = match req.kind {
        RequestType::Post if fm.full_access(&root) => {
            std::fs::create_dir(&dir_path).map_err(io_error)?;
            "create_dir"
        }
        RequestType::Delete if fm.full_access(&root) => {
            if directory.trim_matches('/') == root {
                return Err(ApiError::bad_request("Cannot delete root directory"));
            }
            if !dir_path.is_dir() {
                return Err(ApiError::bad_request(format!("Directory does not exist ({directory})")));
            }
            if req.args.opt_bool("force")?.unwrap_or(false) {
                fm.operation_check(&app, &dir_path)?;
                let p = dir_path.clone();
                tokio::task::spawn_blocking(move || std::fs::remove_dir_all(p))
                    .await
                    .map_err(|e| ApiError::new(500, e.to_string()))?
                    .map_err(|e| ApiError::new(500, e.to_string()))?;
            } else {
                std::fs::remove_dir(&dir_path).map_err(io_error)?;
            }
            clear_metadata(&app, &root, &dir_path, true);
            "delete_dir"
        }
        _ => return Err(ApiError::new(405, "Operation Not Supported")),
    };
    Ok(Value::Object(changed_event(&app, action, &root, &dir_path, None)))
}

async fn move_copy(app: Arc<App>, req: Request) -> ApiResult {
    let fm = &app.files;
    let source = req.args.str("source")?;
    let destination = req.args.str("dest")?;
    let (src_root, src_path) = fm.convert_request_path(&source)?;
    let (dest_root, dest_path) = fm.convert_request_path(&destination)?;
    if !fm.full_access(&dest_root) {
        return Err(ApiError::bad_request(format!("Destination path is read-only: {dest_root}")));
    }
    fm.check_reserved(&src_path, false)?;
    fm.check_reserved(&dest_path, true)?;
    let _guard = fm.op_lock.lock().await;
    if !src_path.exists() {
        return Err(ApiError::bad_request(format!("File {} does not exist", src_path.display())));
    }
    if dest_path.exists() {
        fm.operation_check(&app, &dest_path)?;
    }
    let is_move = req.endpoint == "/server/files/move";
    // As with Python's shutil, a folder destination means "into this folder".
    let into_dir = |dest: &Path| {
        if dest.is_dir() { dest.join(src_path.file_name().unwrap_or_default()) } else { dest.to_path_buf() }
    };
    let (action, full_dest) = if is_move {
        if !fm.full_access(&src_root) {
            return Err(ApiError::bad_request(format!("Source path is read-only, cannot move: {src_root}")));
        }
        fm.operation_check(&app, &src_path)?;
        let action = if src_path.is_dir() { "move_dir" } else { "move_file" };
        (action, into_dir(&dest_path))
    } else if src_path.is_dir() {
        ("create_dir", dest_path.clone())
    } else {
        let action = if into_dir(&dest_path).is_file() { "modify_file" } else { "create_file" };
        (action, into_dir(&dest_path))
    };
    let _own = fm.own_change(&[&src_path, &full_dest]);
    let (from, to) = (src_path.clone(), full_dest.clone());
    let op = tokio::task::spawn_blocking(move || {
        if is_move {
            move_path(&from, &to)
        } else if from.is_dir() {
            copy_tree(&from, &to)
        } else {
            copy_file(&from, &to)
        }
    });
    op.await.map_err(|e| ApiError::new(500, e.to_string()))?.map_err(io_error)?;
    if is_move {
        after_move(&app, &src_root, &dest_root, &src_path, &full_dest).await;
        Ok(Value::Object(changed_event(&app, action, &dest_root, &full_dest, Some((&src_root, &src_path)))))
    } else {
        after_copy(&app, &dest_root, &full_dest).await;
        Ok(Value::Object(changed_event(&app, action, &dest_root, &full_dest, None)))
    }
}

/// `items` as a list, from JSON or a comma-separated string.
fn string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().map(|i| i.as_str().map(str::to_owned).unwrap_or_else(|| i.to_string())).collect(),
        Value::String(s) => s.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect(),
        _ => Vec::new(),
    }
}

async fn zip_files(app: Arc<App>, req: Request) -> ApiResult {
    let fm = &app.files;
    let _guard = fm.op_lock.lock().await;
    let store_only = req.args.opt_bool("store_only")?.unwrap_or(false);
    let dest = match req.args.opt_str("dest")? {
        Some(d) => d,
        None => format!("config/collection-{}.zip", crate::database::timestamp()),
    };
    let (dest_root, dest_path) = fm.convert_request_path(&dest)?;
    if !fm.full_access(&dest_root) {
        return Err(ApiError::bad_request(format!("Destination Root '{dest_root}' is read-only")));
    }
    fm.check_reserved(&dest_path, true)?;
    if dest_path.is_dir() {
        return Err(ApiError::bad_request(format!(
            "Cannot create archive at '{}'.  Path exists as a directory.",
            dest_path.display()
        )));
    }
    if !dest_path.parent().is_some_and(Path::exists) {
        return Err(ApiError::bad_request(format!(
            "Cannot create archive at '{}'.  Parent directory does not exist.",
            dest_path.display()
        )));
    }
    let items = req.args.get("items").map(string_list).unwrap_or_default();
    if items.is_empty() {
        return Err(ApiError::bad_request("At least one file or directory must be specified"));
    }
    let mut sources = Vec::new();
    for item in &items {
        let (root, path) = fm.convert_request_path(item)?;
        fm.check_reserved(&path, false)?;
        if !path.exists() {
            return Err(ApiError::bad_request(format!("No file/directory exits at '{item}'")));
        }
        if !path.is_file() && !path.is_dir() {
            return Err(ApiError::bad_request(format!("Item at path '{item}' is not a valid file or directory")));
        }
        let base = fm.root_path(&root)?.parent().map(Path::to_path_buf).unwrap_or_default();
        let children: Vec<PathBuf> = if path.is_file() {
            vec![path]
        } else {
            // Moonraker packs the files directly in a folder, not its subfolders.
            std::fs::read_dir(&path)
                .map_err(io_error)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && fm.is_reserved(p, false).is_none())
                .collect()
        };
        sources.extend(children.into_iter().map(|p| (p.clone(), base.clone())));
    }
    let _own = fm.own_change(&[&dest_path]);
    let dest_for_task = dest_path.clone();
    tokio::task::spawn_blocking(move || write_zip(&sources, &dest_for_task, store_only))
        .await
        .map_err(|e| ApiError::new(500, e.to_string()))??;
    let info = changed_event(&app, "create_file", &dest_root, &dest_path, None);
    Ok(json!({"destination": info["item"], "action": "zip_files"}))
}

fn write_zip(sources: &[(PathBuf, PathBuf)], dest: &Path, store_only: bool) -> Result<(), ApiError> {
    let fail = |e: &dyn std::fmt::Display| ApiError::new(500, e.to_string());
    let name = dest.file_name().unwrap_or_default();
    let temp = std::env::temp_dir().join(name);
    let file = std::fs::File::create(&temp).map_err(|e| fail(&e))?;
    let mut zip = zip::ZipWriter::new(file);
    let method = if store_only { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
    let mut seen = HashSet::new();
    for (path, base) in sources {
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        let arcname = path.strip_prefix(base).unwrap_or(path).to_string_lossy().into_owned();
        let options = zip::write::SimpleFileOptions::default().compression_method(method);
        let Ok(mut src) = std::fs::File::open(path) else { continue };
        zip.start_file(arcname, options).map_err(|e| fail(&e))?;
        io::copy(&mut src, &mut zip).map_err(|e| fail(&e))?;
    }
    zip.finish().map_err(|e| fail(&e))?;
    move_path(&temp, dest).map_err(|e| fail(&e))
}

async fn delete_request(app: Arc<App>, req: Request) -> ApiResult {
    let path = req.args.str("path")?;
    delete_file(&app, &path).await.map(Value::Object)
}

pub async fn delete_file(app: &Arc<App>, path: &str) -> Result<Map<String, Value>, ApiError> {
    let fm = &app.files;
    let _guard = fm.op_lock.lock().await;
    let (root, full) = fm.convert_request_path(path)?;
    fm.check_reserved(&full, true)?;
    if !fm.full_access(&root) {
        return Err(ApiError::new(405, format!("Path not available for DELETE: {path}")));
    }
    if !full.is_file() {
        return Err(ApiError::bad_request(format!("Invalid file path: {path}")));
    }
    if let Err(e) = fm.operation_check(app, &full)
        && e.status == 403
    {
        return Err(e);
    }
    let _own = fm.own_change(&[&full]);
    std::fs::remove_file(&full).map_err(io_error)?;
    clear_metadata(app, &root, &full, false);
    Ok(changed_event(app, "delete_file", &root, &full, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension("a/b.GCODE"), ".gcode");
        assert_eq!(extension(".hidden"), "");
        assert_eq!(extension("x.tar.gz"), ".gz");
        assert!(is_gcode("sub/part.gco"));
        assert!(!is_gcode("notes.txt"));
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize(Path::new("gcodes/../config/./a")), PathBuf::from("config/a"));
        assert_eq!(normalize(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[test]
    fn request_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths =
            Paths { data: dir.path().to_path_buf(), config_file: dir.path().join("moonraker.conf"), log_file: None };
        let fm = FileManager::new(&Config::parse("").unwrap(), &paths).unwrap();
        let (root, p) = fm.convert_request_path("gcodes/sub/a.gcode").unwrap();
        assert_eq!(root, "gcodes");
        assert_eq!(fm.relative_path("gcodes", &p), "sub/a.gcode");
        assert!(fm.convert_request_path("gcodes/../../etc/passwd").is_err());
        assert!(fm.convert_request_path("nope/a").is_err());
        assert!(fm.check_reserved(&dir.path().join("database/x"), false).is_err());
        assert!(fm.check_reserved(&p, true).is_ok());
    }
}
