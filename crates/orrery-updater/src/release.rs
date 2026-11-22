//! What's particular to orrery's own entry (docs/design/2026-11-22-self-update.md). The
//! entry itself is an executable entry, built by [`crate::config::orrery_entry`]: its asset
//! is an archive of [`PROGRAMS`], checked against [`CHECKSUMS`], and its preflight is
//! `orrery <options> preflight`. This module adds:
//! - the refusal of a development build;
//! - the check after the restart, that orrery runs the version the update put in place;
//! - the options orrery runs with, and its folder;
//! - `orrery-updater self-update` and `rollback`, which do the same from a shell.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::{self, Entry, Kind};
use crate::executable::{self, Fetched, Github, PREVIOUS, Saved};

/// The programs a release holds, in the order they're replaced: orrery last.
pub const PROGRAMS: [&str; 4] = ["orrery-spoolman", "orrery-updater", "orrery-import", "orrery"];
/// The release's checksum file.
pub const CHECKSUMS: &str = "SHA256SUMS";

/// Why this orrery can't update itself, or `None` when it can.
pub fn problem() -> Option<String> {
    (!orrery_build::RELEASE).then(|| "This is a development build, so orrery doesn't update itself.".to_owned())
}

/// Whether `entry` is orrery's own.
pub fn is_own(entry: &Entry) -> bool {
    matches!(&entry.kind, Kind::Executable(o) if o.orrery)
}

/// The warning for an update or a rollback that hasn't taken effect: orrery runs another
/// version than the one it put in place.
pub fn not_restarted(saved: &Saved) -> Option<String> {
    let expected = saved.expected_version.as_deref()?;
    (expected != orrery_build::VERSION).then(|| {
        format!(
            "orrery put {expected} in place, but runs {}. It takes effect when orrery restarts: sudo systemctl restart orrery",
            orrery_build::VERSION
        )
    })
}

/// The check after the restart that follows an update or a rollback: when orrery runs the
/// version it put in place, the record goes, and the log says so.
pub fn restarted(saved: &mut Saved) {
    if saved.expected_version.as_deref() == Some(orrery_build::VERSION) {
        tracing::info!("orrery runs {}, which the last update or rollback put in place", orrery_build::VERSION);
        saved.expected_version = None;
    }
}

/// Adds what's particular to orrery's entry to its executable status: a development build
/// isn't valid, and a version not yet running is a warning.
pub fn adjust_status(status: &mut Value, saved: &Saved) {
    let problem = problem();
    if problem.is_some() {
        status["is_valid"] = json!(false);
    }
    if let Some(warnings) = status["warnings"].as_array_mut() {
        warnings.extend(problem.into_iter().chain(not_restarted(saved)).map(Value::from));
    }
}

/// The options orrery runs with, from the command line of the agent's parent, which is
/// orrery. The program's own name is left out. The preflight runs in another folder, so
/// relative paths are made absolute against orrery's.
pub fn orrery_args() -> Vec<String> {
    let parent = rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get());
    let cwd = std::fs::read_link(format!("/proc/{parent}/cwd")).unwrap_or_default();
    let words: Vec<String> = std::fs::read(format!("/proc/{parent}/cmdline"))
        .map(|raw| {
            raw.split(|b| *b == 0)
                .skip(1)
                .filter(|w| !w.is_empty())
                .map(|w| String::from_utf8_lossy(w).into_owned())
                .collect()
        })
        .unwrap_or_default();
    absolute_paths(&words, &cwd)
}

/// orrery's options that take a path.
const PATH_OPTIONS: [&str; 8] = ["-d", "--datapath", "-c", "--configfile", "--orrery-config", "-l", "--logfile", "-u"];

/// `words` with each path option's relative path joined to `cwd`. `~` paths stay, since
/// orrery expands them itself.
fn absolute_paths(words: &[String], cwd: &Path) -> Vec<String> {
    let fix = |path: &str| {
        if path.starts_with('~') || Path::new(path).is_absolute() {
            path.to_owned()
        } else {
            cwd.join(path).to_string_lossy().into_owned()
        }
    };
    let mut out = Vec::with_capacity(words.len());
    let mut takes_path = false;
    for word in words {
        if takes_path {
            out.push(fix(word));
            takes_path = false;
        } else if let Some((flag, path)) = word.split_once('=')
            && PATH_OPTIONS.contains(&flag)
        {
            out.push(format!("{flag}={}", fix(path)));
        } else {
            takes_path = PATH_OPTIONS.contains(&word.as_str()) || word == "--unixsocket";
            out.push(word.clone());
        }
    }
    out
}

/// orrery's folder: this program's, which orrery starts its agents from. When this program
/// is the copy in `.previous/`, it's the folder above.
pub fn own_dir() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    match (dir.file_name(), dir.parent()) {
        (Some(name), Some(parent)) if name == PREVIOUS => parent.to_path_buf(),
        _ => dir,
    }
}

/// orrery's entry for the shell commands, which have no config: `repo` as `--repo` gives it.
fn shell_entry(repo: Option<String>, args: &[String]) -> Result<Entry, String> {
    let mut map = Map::new();
    let mut from_toml = Vec::new();
    if let Some(repo) = repo {
        map.insert("repo".into(), Value::String(repo));
        from_toml.push("repo".to_owned());
    }
    let global = config::global(&Map::new(), &mut Vec::new());
    let entry = config::orrery_entry(own_dir(), &map, &from_toml, args, &global, &mut Vec::new())?;
    Ok(entry.expect("enabled without a section"))
}

/// Runs `work`, and prints the messages it sends while it runs.
async fn printing<T>(work: impl AsyncFnOnce(&crate::packagekit::Messages) -> T) -> T {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let work = async move { work(&tx).await };
    let print = async {
        while let Some(line) = rx.recv().await {
            println!("{line}");
        }
    };
    tokio::join!(work, print).0
}

/// What the shell commands say after they replace the programs.
const RESTART: &str = "It takes effect when orrery restarts. If a print runs, wait for it to end, then run:\n  sudo systemctl restart orrery";

/// `orrery-updater self-update`: installs the latest release, or the one tagged `tag`, without
/// orrery. `args` are orrery's options, for the preflight.
pub async fn self_update(repo: Option<String>, tag: Option<String>, args: &[String]) -> Result<(), String> {
    if let Some(problem) = problem() {
        return Err(problem);
    }
    let entry = shell_entry(repo, args)?;
    let Kind::Executable(opts) = &entry.kind else { unreachable!("an executable entry") };
    let installed = executable::installed(opts).await;
    if !installed.valid {
        return Err(format!(
            "orrery can't update the programs in {}: {}",
            opts.path.display(),
            installed.warnings.join(" ")
        ));
    }
    let gh = Github::new()?;
    let release = match gh.release(&installed.repo, entry.channel, tag.as_deref(), None).await? {
        Fetched::Release(r, _) if r.is_object() => r,
        _ => {
            let which = tag.as_deref().unwrap_or("latest");
            return Err(format!("GitHub has no release {which} in {}", installed.repo));
        }
    };
    let (version, asset, _) = executable::read_release(&release, installed.asset_name.as_deref());
    if tag.is_none() && version == installed.version {
        println!("orrery {version} is the latest release, and it's installed.");
        return Ok(());
    }
    let name = installed.asset_name.clone().unwrap_or_default();
    let asset = asset.ok_or_else(|| format!("Release {version} has no {name}"))?;
    let checksums = executable::checksums_of(&entry, &release);
    println!("Installing orrery {version} in {}...", opts.path.display());
    printing(async |out| {
        executable::install(&gh, &entry, &installed, &asset, checksums.as_ref(), &version, "orrery", out).await
    })
    .await?;
    println!("Installed orrery {version}. {RESTART}");
    Ok(())
}

/// `orrery-updater rollback`: puts `.previous/` back, without orrery.
pub async fn rollback(args: &[String]) -> Result<(), String> {
    let entry = shell_entry(None, args)?;
    let Kind::Executable(opts) = &entry.kind else { unreachable!("an executable entry") };
    let installed = executable::installed(opts).await;
    let files = executable::files(opts, &installed);
    let Some(version) = executable::previous_version(&opts.path, &files) else {
        return Err(format!(
            "{} holds no complete set of orrery's programs, so there's nothing to roll back to.\nTo install a given release instead, run:\n  orrery self-update --version <version>",
            opts.path.join(PREVIOUS).display()
        ));
    };
    printing(async |out| executable::restore_previous(&entry, &files, out).await).await?;
    println!("Restored orrery {version}. {RESTART}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_status() {
        let mut status = json!({"is_valid": true, "warnings": []});
        let mut saved = Saved::default();
        adjust_status(&mut status, &saved);
        if orrery_build::RELEASE {
            assert_eq!(status, json!({"is_valid": true, "warnings": []}));
        } else {
            assert_eq!(status["is_valid"], false);
            assert_eq!(status["warnings"], json!(["This is a development build, so orrery doesn't update itself."]));
        }
        // Put in place, not yet running.
        saved.expected_version = Some("v9.9.9".into());
        let w = not_restarted(&saved).unwrap();
        assert!(w.starts_with("orrery put v9.9.9 in place, but runs v"), "{w}");
        saved.expected_version = Some(orrery_build::VERSION.into());
        assert_eq!(not_restarted(&saved), None);
    }

    #[test]
    fn paths_made_absolute() {
        let words = [
            "-c",
            "config/moonraker.conf",
            "--orrery-config=o.toml",
            "-d",
            "~/printer_data",
            "-n",
            "-u",
            "/run/m.sock",
        ]
        .map(str::to_owned);
        assert_eq!(
            absolute_paths(&words, Path::new("/home/pi")),
            [
                "-c",
                "/home/pi/config/moonraker.conf",
                "--orrery-config=/home/pi/o.toml",
                "-d",
                "~/printer_data",
                "-n",
                "-u",
                "/run/m.sock"
            ]
        );
    }
}
