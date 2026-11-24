//! `archive` entries, orrery's own type: every file of a release's archive, installed into the
//! entry's folder, with a record of what was installed (docs/design/2026-11-23-archive-entries.md).
//!
//! An update unpacks the download beside the files, checks it, and keeps the old set in
//! `.previous/`, so a rollback needs no download. Files the new set doesn't have are removed;
//! files orrery never installed, such as a config the program writes beside itself, aren't
//! touched.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::config::{ArchiveFormat, ArchiveOptions, Entry, Kind, Source};
use crate::executable::{Asset, Github, Installed, PREVIOUS, expected_sum, sha256};
use crate::packagekit::Messages;

/// The record of what orrery installed, in the entry's folder and in `.previous/`.
pub const RECORD: &str = ".release-info.json";

/// What orrery installed: the release's tag, where it came from, and its files, as paths
/// relative to the entry's folder. It's stored as Moonraker's `release_info.json`, with
/// `files` added; its own name keeps it apart from a `release_info.json` the package ships.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub repo: String,
    pub version: String,
    pub asset: String,
    pub files: Vec<String>,
}

impl Record {
    fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(RECORD)).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let files = v["files"].as_array()?.iter().map(|f| f.as_str().map(str::to_owned)).collect::<Option<_>>()?;
        Some(Self {
            repo: format!("{}/{}", v["project_owner"].as_str()?, v["project_name"].as_str()?),
            version: v["version"].as_str()?.to_owned(),
            asset: v["asset_name"].as_str()?.to_owned(),
            files,
        })
    }

    /// Writes the record beside the old one, then renames it over it.
    fn write(&self, dir: &Path) -> Result<(), String> {
        let path = dir.join(RECORD);
        let partial = dir.join(format!("{RECORD}.new"));
        let (owner, name) = self.repo.split_once('/').unwrap_or(("", &self.repo));
        let text = json!({
            "project_owner": owner,
            "project_name": name,
            "version": self.version,
            "asset_name": self.asset,
            "files": self.files,
        });
        std::fs::write(&partial, text.to_string()).map_err(|e| format!("Can't write {}: {e}", partial.display()))?;
        std::fs::rename(&partial, &path).map_err(|e| format!("Can't write {}: {e}", path.display()))
    }
}

/// What's installed, from the record. Without one, such as before orrery's first update of a
/// program installed by hand, the version is `?`, and the entry offers the newest release.
///
/// For orrery's own entry, the repository is the one the install came from, unless
/// `orrery.toml` names one: the record's, or else the one `release_info.json` names, so a
/// fork's install keeps taking the fork's releases.
pub async fn installed(opts: &ArchiveOptions) -> Installed {
    let Source::Github { repo, .. } = &opts.source;
    let mut i = Installed {
        valid: true,
        version: "?".into(),
        asset_name: Some(opts.source.asset().to_owned()),
        repo: repo.clone(),
        warnings: Vec::new(),
        anomalies: Vec::new(),
    };
    let path = &opts.path;
    if path.exists() && !path.is_dir() {
        i.warnings.push(format!("Location at option 'path: {}' is not a directory.", path.display()));
        i.valid = false;
    } else if path.is_dir() && rustix::fs::access(path.as_path(), rustix::fs::Access::WRITE_OK).is_err() {
        i.warnings.push(format!("Location at option 'path: {}' is not writable.", path.display()));
        i.valid = false;
    } else if let Some(record) = Record::read(path) {
        i.version = record.version;
        if opts.orrery && !opts.repo_pinned {
            i.repo = record.repo;
        }
    } else if opts.orrery
        && !opts.repo_pinned
        && let Some(repo) = release_info_repo(path)
    {
        i.repo = repo;
    }
    if opts.orrery && opts.source.asset().is_empty() {
        i.warnings.push(format!("orrery has no releases for this computer's CPU, {}", orrery_build::TARGET));
        i.valid = false;
    }
    i
}

/// `owner/name`, from the `release_info.json` that orrery's releases bring.
fn release_info_repo(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path.join("release_info.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(format!("{}/{}", v["project_owner"].as_str()?, v["project_name"].as_str()?))
}

/// The version `.previous/` holds, for a rollback without a download.
pub fn previous_version(path: &Path) -> Option<String> {
    Record::read(&path.join(PREVIOUS)).map(|r| r.version)
}

/// Installs the archive `asset` as release `version`, from `repo`, which the record keeps.
///
/// The download goes to a hidden folder beside the files, `.<entry>.update`. There it's
/// checked against `checksums` when the entry names a checksum file, unpacked, and its
/// preflight runs. Then the old set is hard-linked into `.previous/`, the new files are renamed
/// into place, and the files only the old set had are removed. Before the renames, a failure
/// leaves every file as it was.
#[allow(clippy::too_many_arguments)]
pub async fn install(
    gh: &Github,
    entry: &Entry,
    repo: &str,
    asset: &Asset,
    checksums: Option<&Asset>,
    version: &str,
    application: &str,
    out: &Messages,
) -> Result<(), String> {
    let Kind::Archive(opts) = &entry.kind else { return Err("Not an archive entry".into()) };
    let Source::Github { asset: name, checksums: sums_name, .. } = &opts.source;
    let prefix = entry.prefix();
    let path = &opts.path;
    tokio::fs::create_dir_all(path).await.map_err(|e| format!("Can't make {}: {e}", path.display()))?;
    let staging = path.join(format!(".{}.update", entry.name));
    let _ = tokio::fs::remove_dir_all(&staging).await;
    tokio::fs::create_dir(&staging).await.map_err(|e| format!("Can't make {}: {e}", staging.display()))?;
    let result = async {
        let download = staging.join("download");
        gh.download(asset, &download, application, out).await?;
        if let Some(sums_name) = sums_name {
            let sums = checksums.ok_or_else(|| {
                format!("Release {version} has no {sums_name}, so orrery can't check the download.")
            })?;
            let sums_file = staging.join("checksums");
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
        let _ = out.send(format!("{prefix}Download Complete, extracting release to '{}'", path.display()));
        let unpacked = staging.join("files");
        tokio::fs::create_dir(&unpacked).await.map_err(|e| format!("Can't make {}: {e}", unpacked.display()))?;
        unpack(&download, opts.format, &unpacked).await?;
        let (wrapper, asset_name) = (opts.wrapper_folders, name.clone());
        let (root, mut files) = tokio::task::spawn_blocking(move || new_set(&unpacked, wrapper, &asset_name))
            .await
            .map_err(|e| format!("Can't read the archive: {e}"))??;
        // orrery's own programs are replaced before it, so a failure partway leaves the old
        // orrery, which runs with either set.
        if let Some(at) = files.iter().position(|f| opts.orrery && f == "orrery") {
            let orrery = files.remove(at);
            files.push(orrery);
        }
        check(entry, &files, &root, path, out).await?;
        let old = Record::read(path);
        let record = Record { repo: repo.to_owned(), version: version.to_owned(), asset: name.clone(), files };
        let path = path.clone();
        tokio::task::spawn_blocking(move || put_in_place(&path, &root, old.as_ref(), &record))
            .await
            .map_err(|e| format!("Can't put the new files in place: {e}"))?
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&staging).await;
    result
}

/// Puts the set in `.previous/` back, after its preflight, and removes the files only the newer
/// set had.
pub async fn restore_previous(entry: &Entry, out: &Messages) -> Result<(), String> {
    let Kind::Archive(opts) = &entry.kind else { return Err("Not an archive entry".into()) };
    let previous = opts.path.join(PREVIOUS);
    let record = Record::read(&previous).ok_or_else(|| {
        format!("{} holds no earlier release, so there's nothing to roll back to.", previous.display())
    })?;
    check(entry, &record.files, &previous, &opts.path, out).await?;
    let path = opts.path.clone();
    tokio::task::spawn_blocking(move || {
        let current = Record::read(&path);
        for file in &record.files {
            place(&previous.join(file), &path, file)?;
        }
        remove_dropped(&path, current.as_ref(), &record.files);
        record.write(&path)?;
        let _ = std::fs::remove_dir_all(&previous);
        Ok(())
    })
    .await
    .map_err(|e| format!("Can't put the old files back: {e}"))?
}

/// Unpacks the download with the system's `tar`, as `format` says.
async fn unpack(archive: &Path, format: ArchiveFormat, into: &Path) -> Result<(), String> {
    let flag = match format {
        ArchiveFormat::TarGz => "-xzf",
        ArchiveFormat::TarXz => "-xJf",
    };
    let out = tokio::process::Command::new("tar")
        .arg(flag)
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
        let what = String::from_utf8_lossy(&out.stderr);
        Err(format!("Can't unpack the download as {}: {}", format.as_str(), what.trim()))
    }
}

/// The folder the archive's files are in, past its wrapper folders, and those files. Without a
/// count, an archive that holds only one folder has one wrapper folder. With one, each level
/// must be exactly one folder.
fn new_set(unpacked: &Path, wrapper_folders: Option<usize>, asset: &str) -> Result<(PathBuf, Vec<String>), String> {
    let levels = match wrapper_folders {
        Some(n) => n,
        None => usize::from(only_folder(&list(unpacked)?).is_some()),
    };
    let mut root = unpacked.to_path_buf();
    for level in 0..levels {
        let entries = list(&root)?;
        let Some(folder) = only_folder(&entries) else {
            let place = if level == 0 {
                "at its top".to_owned()
            } else {
                format!("in {}/", root.strip_prefix(unpacked).unwrap_or(&root).display())
            };
            let folders = if levels == 1 { "folder" } else { "folders" };
            return Err(format!(
                "{asset} doesn't have {levels} wrapper {folders}: it has {} {place}, so nothing was changed.",
                describe(&entries)
            ));
        };
        root = folder;
    }
    let mut files = Vec::new();
    walk(&root, &root, &mut files)?;
    if files.is_empty() {
        return Err(format!("{asset} has no files, so nothing was changed."));
    }
    files.sort();
    Ok((root, files))
}

/// A folder's entries, with whether each is a folder.
fn list(dir: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("Can't read {}: {e}", dir.display()))?;
    let mut entries = Vec::new();
    for e in read {
        let e = e.map_err(|e| format!("Can't read {}: {e}", dir.display()))?;
        let kind = e.file_type().map_err(|e| format!("Can't read {}: {e}", e))?;
        entries.push((e.path(), kind.is_dir()));
    }
    Ok(entries)
}

fn only_folder(entries: &[(PathBuf, bool)]) -> Option<PathBuf> {
    match entries {
        [(path, true)] => Some(path.clone()),
        _ => None,
    }
}

/// "2 folders and 1 file", or "nothing".
fn describe(entries: &[(PathBuf, bool)]) -> String {
    let folders = entries.iter().filter(|(_, dir)| *dir).count();
    let files = entries.len() - folders;
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (folders, files) {
        (0, 0) => "nothing".into(),
        (0, f) => count(f, "file", "files"),
        (d, 0) => count(d, "folder", "folders"),
        (d, f) => format!("{} and {}", count(d, "folder", "folders"), count(f, "file", "files")),
    }
}

/// Every file under `dir`, relative to `root`. A link or another special file stops the update.
fn walk(root: &Path, dir: &Path, files: &mut Vec<String>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("Can't read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("Can't read {}: {e}", dir.display()))?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        let kind = entry.file_type().map_err(|e| format!("Can't read {relative}: {e}"))?;
        if kind.is_dir() {
            walk(root, &path, files)?;
        } else if kind.is_file() {
            files.push(relative);
        } else {
            return Err(format!("The archive has {relative}, which isn't a file or a folder, so nothing was changed."));
        }
    }
    Ok(())
}

/// The entry's preflight, run from the set in `dir`, in the entry's folder. The first word
/// names a file of the set.
async fn check(entry: &Entry, files: &[String], dir: &Path, path: &Path, out: &Messages) -> Result<(), String> {
    let Some(first) = entry.preflight.first() else { return Ok(()) };
    if !files.contains(first) {
        return Err(format!(
            "The preflight's first word, {first}, isn't a file of the archive, so nothing was changed."
        ));
    }
    let _ = out.send(format!("{}Running the preflight check...", entry.prefix()));
    // The folder may not be there yet, before the first install.
    let cwd = if path.is_dir() { path } else { dir };
    crate::preflight::run(first, &dir.join(first), &entry.preflight[1..], cwd, crate::preflight::LIMIT).await
}

/// Keeps the old set in `.previous/`, renames the new files into place, removes the files only
/// the old set had, and writes the record.
fn put_in_place(path: &Path, root: &Path, old: Option<&Record>, record: &Record) -> Result<(), String> {
    if let Some(old) = old {
        keep_previous(path, old)?;
    }
    for file in &record.files {
        place(&root.join(file), path, file)?;
    }
    remove_dropped(path, old, &record.files);
    record.write(path)
}

/// Hard-links the old set and its record into `.previous/`, replacing what an earlier update
/// kept there. They're the same files until the update renames new ones over them.
fn keep_previous(path: &Path, old: &Record) -> Result<(), String> {
    let previous = path.join(PREVIOUS);
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::create_dir(&previous).map_err(|e| format!("Can't make {}: {e}", previous.display()))?;
    for file in old.files.iter().map(String::as_str).chain([RECORD]) {
        let (from, to) = (path.join(file), previous.join(file));
        if !from.is_file() {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Can't make {}: {e}", parent.display()))?;
        }
        std::fs::hard_link(&from, &to)
            .map_err(|e| format!("Can't keep the old {file} in {}: {e}", previous.display()))?;
    }
    Ok(())
}

/// Renames `from` to `file` in `path`, making its folders.
fn place(from: &Path, path: &Path, file: &str) -> Result<(), String> {
    let to = path.join(file);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Can't make {}: {e}", parent.display()))?;
    }
    std::fs::rename(from, &to).map_err(|e| format!("Can't put the new {file} in place: {e}"))
}

/// Removes the files `old` had and `files` doesn't, and the folders that leaves empty.
fn remove_dropped(path: &Path, old: Option<&Record>, files: &[String]) {
    let Some(old) = old else { return };
    for file in old.files.iter().filter(|f| !files.contains(f)) {
        let target = path.join(file);
        if std::fs::remove_file(&target).is_err() {
            continue;
        }
        let mut dir = target.parent();
        while let Some(d) = dir.filter(|d| *d != path && d.starts_with(path)) {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{Map, json};

    use super::*;
    use crate::config;
    use crate::executable::{Saved, refresh, status, tests::server};

    /// An archive with `files`, each a script that prints its name and `tag`, in `layout`'s
    /// folder, packed as `format`.
    fn archive(tag: &str, layout: &str, files: &[&str], format: &str) -> Vec<u8> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().join("top");
        for file in files {
            let path = top.join(layout).join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("#!/bin/sh\necho {file} {tag}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let file = dir.path().join("a");
        let flag = if format == "tar.xz" { "-cJf" } else { "-czf" };
        let status =
            std::process::Command::new("tar").arg(flag).arg(&file).arg("-C").arg(&top).arg(".").status().unwrap();
        assert!(status.success());
        std::fs::read(file).unwrap()
    }

    fn entry(dir: &Path, extra: serde_json::Value) -> Entry {
        let g = config::global(&Map::new(), &mut Vec::new());
        let mut map = json!({"type": "archive", "path": dir, "github": "mjonuschat/aldis",
            "asset": "aldis-{arch}-linux.tar.xz", "format": "tar.xz"});
        map.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let map = map.as_object().unwrap();
        let mut e = config::entry("aldis", map, &g).unwrap();
        let keys: Vec<String> = map.keys().cloned().collect();
        config::own_options("update_manager aldis", map, &keys, &mut e, &mut Vec::new()).unwrap();
        e
    }

    fn opts(e: &Entry) -> &ArchiveOptions {
        let Kind::Archive(o) = &e.kind else { panic!("an archive") };
        o
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[tokio::test]
    async fn installs_updates_and_rolls_back_a_set_of_files() {
        let machine = rustix::system::uname().machine().to_string_lossy().into_owned();
        let asset = format!("aldis-{machine}-linux.tar.xz");
        let folder = format!("aldis-{machine}-linux");
        let v1 = archive("v0.3.1", &folder, &["aldis", "LICENSE", "share/old"], "tar.xz");
        let v2 = archive("v0.3.2", &folder, &["aldis", "LICENSE", "aldis-helper"], "tar.xz");
        let base = server(|base| {
            let latest = json!({"tag_name": "v0.3.2", "assets": [
                {"name": "aldis-riscv64-linux.tar.xz", "browser_download_url": format!("{base}dl/other")},
                {"name": asset, "browser_download_url": format!("{base}dl/v2"), "size": v2.len()},
            ]});
            HashMap::from([
                ("/dl/v1".to_owned(), v1.clone()),
                ("/dl/v2".to_owned(), v2.clone()),
                ("/repos/mjonuschat/aldis/releases/latest".to_owned(), latest.to_string().into_bytes()),
            ])
        })
        .await;
        let gh = Github::with_api(&base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aldis");
        let mut e = entry(&path, json!({"wrapper_folders": 1}));
        e.preflight = vec!["aldis".into(), "--version".into()];
        assert_eq!(opts(&e).source.asset(), asset);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let at = |p: &str| Asset { url: format!("{base}dl/{p}"), content_type: "?".into(), size: 0 };

        // Nothing installed yet: the version is unknown, and the newest release is offered.
        // Clients see the folder is missing, with a version older than any release.
        let i = installed(opts(&e)).await;
        assert!(i.valid && i.version == "?", "{i:?}");
        assert_eq!(status(&e, &i, &Saved::default())["version"], "v0.0.0-missing");
        let mut saved = Saved::default();
        refresh(&gh, &e, &i, &mut saved).await;
        assert_eq!(saved.remote_version, "v0.3.2");
        assert!(saved.asset.as_ref().unwrap().url.ends_with("/dl/v2"));
        assert_eq!(status(&e, &i, &saved)["configured_type"], "archive");

        // The first install makes the folder, and records the set.
        install(&gh, &e, "mjonuschat/aldis", &at("v1"), None, "v0.3.1", "aldis", &tx).await.unwrap();
        assert_eq!(read(&path.join("aldis")), "#!/bin/sh\necho aldis v0.3.1\n");
        assert_eq!(read(&path.join("share/old")), "#!/bin/sh\necho share/old v0.3.1\n");
        assert_eq!(installed(opts(&e)).await.version, "v0.3.1");
        assert!(!path.join(PREVIOUS).exists() && !path.join(".aldis.update").exists());
        std::fs::write(path.join("config.toml"), "mine").unwrap();

        // An update adds the new file, removes the dropped one and its empty folder, and
        // keeps the old set in .previous/. A file orrery didn't install stays.
        install(&gh, &e, "mjonuschat/aldis", saved.asset.as_ref().unwrap(), None, "v0.3.2", "aldis", &tx)
            .await
            .unwrap();
        assert_eq!(read(&path.join("aldis")), "#!/bin/sh\necho aldis v0.3.2\n");
        assert!(path.join("aldis-helper").is_file());
        assert!(!path.join("share").exists());
        assert_eq!(read(&path.join("config.toml")), "mine");
        assert_eq!(installed(opts(&e)).await.version, "v0.3.2");
        assert_eq!(previous_version(&path).as_deref(), Some("v0.3.1"));
        assert_eq!(read(&path.join(PREVIOUS).join("share/old")), "#!/bin/sh\necho share/old v0.3.1\n");
        assert_eq!(status(&e, &installed(opts(&e)).await, &saved)["rollback_version"], "v0.3.1");

        // A rollback puts the old set back, without the new file.
        restore_previous(&e, &tx).await.unwrap();
        assert_eq!(read(&path.join("aldis")), "#!/bin/sh\necho aldis v0.3.1\n");
        assert!(path.join("share/old").is_file() && !path.join("aldis-helper").exists());
        assert_eq!(installed(opts(&e)).await.version, "v0.3.1");
        assert_eq!(previous_version(&path), None);
        assert_eq!(read(&path.join("config.toml")), "mine");
    }

    /// orrery's own entry: whatever its release's archive holds, with `orrery` replaced last,
    /// and the repository the install came from.
    #[tokio::test]
    async fn own_entry() {
        let programs = ["orrery-import", "orrery-spoolman", "orrery", "LICENSE"];
        let v2 = archive(
            "v0.2.0",
            "",
            &["orrery-gpio", "orrery-spoolman", "orrery", "LICENSE", "release_info.json"],
            "tar.gz",
        );
        let sums = {
            use sha2::{Digest, Sha256};
            let sum: String = Sha256::digest(&v2).iter().map(|b| format!("{b:02x}")).collect();
            format!("{sum}  orrery-aarch64.tar.gz\n")
        };
        let base = server(|_| {
            HashMap::from([("/dl/v2".to_owned(), v2.clone()), ("/dl/sums".to_owned(), sums.clone().into_bytes())])
        })
        .await;
        let gh = Github::with_api(&base).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        // A v0.1.x install: the installer's files, and release_info.json, without a record.
        for file in programs {
            std::fs::write(path.join(file), format!("{file} v0.1.2")).unwrap();
        }
        let info = r#"{"project_name":"orrery","project_owner":"someone","version":"v0.1.2","asset_name":"orrery-aarch64.tar.gz"}"#;
        std::fs::write(path.join("release_info.json"), info).unwrap();
        let g = config::global(&Map::new(), &mut Vec::new());
        let own = |map: serde_json::Value, from: &[&str]| {
            let from: Vec<String> = from.iter().map(|k| (*k).to_owned()).collect();
            let mut e = config::orrery_entry(path.into(), map.as_object().unwrap(), &from, &[], &g, &mut Vec::new())
                .unwrap()
                .unwrap();
            // This test's build may be for a CPU orrery doesn't release for.
            if let Kind::Archive(o) = &mut e.kind {
                let Source::Github { asset, .. } = &mut o.source;
                *asset = "orrery-aarch64.tar.gz".into();
            }
            e
        };
        let e = own(json!({}), &[]);

        // Without a record, the version is unknown, so the newest release is offered, from the
        // repository the install came from.
        let i = installed(opts(&e)).await;
        assert!(i.valid, "{:?}", i.warnings);
        assert_eq!((i.version.as_str(), i.repo.as_str()), ("?", "someone/orrery"));
        // orrery.toml's repo wins.
        let pinned = own(json!({"repo": "other/orrery"}), &["repo"]);
        assert_eq!(installed(opts(&pinned)).await.repo, "other/orrery");

        // The update installs every file of the archive, orrery last, and records them, with
        // the repository. It runs the preflight from the new set.
        let at = |p: &str| Asset { url: format!("{base}dl/{p}"), content_type: "?".into(), size: 0 };
        install(&gh, &e, &i.repo, &at("v2"), Some(&at("sums")), "v0.2.0", "orrery", &tx).await.unwrap();
        let record = Record::read(path).unwrap();
        assert_eq!(record.files.last().map(String::as_str), Some("orrery"));
        assert!(record.files.contains(&"orrery-gpio".to_owned()));
        // In Moonraker's format, with the files added.
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(path.join(RECORD)).unwrap()).unwrap();
        assert_eq!(
            (saved["project_owner"].as_str(), saved["asset_name"].as_str()),
            (Some("someone"), Some(record.asset.as_str()))
        );
        assert_eq!(record.repo, "someone/orrery");
        assert!(path.join("orrery-gpio").is_file());
        let now = installed(opts(&e)).await;
        assert_eq!((now.version.as_str(), now.repo.as_str()), ("v0.2.0", "someone/orrery"));
        // Without a record before it, the update doesn't know what to remove, nor what to keep.
        assert!(path.join("orrery-import").is_file());
        assert_eq!(previous_version(path), None);

        assert_eq!(status(&e, &now, &Saved::default())["configured_type"], "archive");
        // Without a record, clients see a version older than any release, so they offer one.
        assert_eq!(status(&e, &i, &Saved::default())["version"], "v0.0.0-invalid");
    }

    #[tokio::test]
    async fn checks_the_layout_and_the_preflight() {
        let flat = archive("v1", "", &["aldis", "LICENSE"], "tar.gz");
        let wrapped = archive("v1", "aldis-x", &["aldis"], "tar.gz");
        let base = server(|_| {
            HashMap::from([("/dl/flat".to_owned(), flat.clone()), ("/dl/wrapped".to_owned(), wrapped.clone())])
        })
        .await;
        let gh = Github::with_api(&base).unwrap();
        let at = |p: &str| Asset { url: format!("{base}dl/{p}"), content_type: "?".into(), size: 0 };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aldis");
        let gz = json!({"format": "tar.gz", "asset": "aldis.tar.gz"});
        let with = |extra: serde_json::Value| {
            let mut m = gz.as_object().unwrap().clone();
            m.extend(extra.as_object().unwrap().clone());
            entry(&path, serde_json::Value::Object(m))
        };

        // Explicit wrapper folders are strict.
        let err = install(
            &gh,
            &with(json!({"wrapper_folders": 1})),
            "mjonuschat/aldis",
            &at("flat"),
            None,
            "v1",
            "aldis",
            &tx,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "aldis.tar.gz doesn't have 1 wrapper folder: it has 2 files at its top, so nothing was changed."
        );
        assert!(!path.join("aldis").exists());
        // 0 keeps a lone folder as it is.
        install(
            &gh,
            &with(json!({"wrapper_folders": 0})),
            "mjonuschat/aldis",
            &at("wrapped"),
            None,
            "v1",
            "aldis",
            &tx,
        )
        .await
        .unwrap();
        assert!(path.join("aldis-x/aldis").is_file());
        // Unset, a lone folder is a wrapper folder, and a flat archive has none.
        install(&gh, &with(json!({})), "mjonuschat/aldis", &at("wrapped"), None, "v2", "aldis", &tx).await.unwrap();
        assert!(path.join("aldis").is_file() && !path.join("aldis-x").exists());
        install(&gh, &with(json!({})), "mjonuschat/aldis", &at("flat"), None, "v3", "aldis", &tx).await.unwrap();
        assert!(path.join("LICENSE").is_file());

        // The preflight names a file of the set, and a failing one changes nothing.
        let mut e = with(json!({}));
        e.preflight = vec!["missing".into()];
        let err = install(&gh, &e, "mjonuschat/aldis", &at("flat"), None, "v4", "aldis", &tx).await.unwrap_err();
        assert_eq!(err, "The preflight's first word, missing, isn't a file of the archive, so nothing was changed.");
        assert_eq!(installed(opts(&e)).await.version, "v3");

        // The wrong format is an error from tar.
        let err =
            install(&gh, &with(json!({"format": "tar.xz"})), "mjonuschat/aldis", &at("flat"), None, "v5", "aldis", &tx)
                .await
                .unwrap_err();
        assert!(err.starts_with("Can't unpack the download as tar.xz:"), "{err}");
        assert_eq!(installed(opts(&e)).await.version, "v3");
    }

    #[test]
    fn only_from_orrery_toml() {
        let g = config::global(&Map::new(), &mut Vec::new());
        let map =
            json!({"type": "archive", "path": "/x", "github": "a/b", "asset": "b-{arch}.tar.gz", "format": "tar.gz"});
        let map = map.as_object().unwrap();
        let mut e = config::entry("b", map, &g).unwrap();
        let err = config::own_options("update_manager b", map, &[], &mut e, &mut Vec::new()).unwrap_err();
        assert_eq!(
            err,
            "[update_manager b]: Moonraker has no type archive, so orrery leaves this entry in moonraker.conf out, as Moonraker would. To use the type, move the entry to orrery.toml."
        );
        let bad = json!({"type": "archive", "path": "/x", "github": "a/b", "asset": "b", "format": "zip"});
        let err = config::entry("b", bad.as_object().unwrap(), &g).unwrap_err();
        assert_eq!(err, r#"[update_manager b] format: expected tar.gz or tar.xz, got "zip""#);
    }
}
