//! Plugins in Klipper's own folder: files that other update entries link into `klippy/extras`,
//! or into Kalico's `klippy/plugins`, and files there that no entry updates
//! (docs/design/2026-11-22-klipper-updates.md, "Plugins in Klipper's folder").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::git;

/// Kalico's folder for plugins. Git ignores it, so its files are never untracked.
const PLUGINS_DIR: &str = "klippy/plugins";

/// What's in Klipper's folder besides Klipper's own files.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Links {
    /// Each entry's links, by the entry's name: paths in Klipper's repo.
    pub by_entry: BTreeMap<String, Vec<String>>,
    /// Untracked files that no entry's repo holds, such as a plugin copied in by hand.
    pub unknown: Vec<String>,
    /// Every untracked file, for the collision check.
    pub untracked: Vec<String>,
}

impl Links {
    /// The entry a file in Klipper's repo belongs to.
    pub fn owner(&self, file: &str) -> Option<&str> {
        self.by_entry.iter().find(|(_, files)| files.iter().any(|f| f == file)).map(|(name, _)| name.as_str())
    }
}

/// Reads Klipper's repo at `klipper`, and sorts its untracked files, and the links in Kalico's
/// plugin folder, by the entry whose repo they resolve into. `entries` are the other `git_repo`
/// entries: their names and paths.
pub async fn find(klipper: &Path, entries: &[(String, PathBuf)]) -> Links {
    let untracked: Vec<String> =
        git::git(klipper, &["ls-files", "--others", "--exclude-standard"], git::COMMAND_TIMEOUT)
            .await
            .map(|text| text.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
    let in_plugins_dir = plugin_dir_links(klipper);
    // An entry whose folder holds Klipper's own isn't a plugin.
    let klipper_real = std::fs::canonicalize(klipper).unwrap_or_else(|_| klipper.to_path_buf());
    let roots: Vec<(&str, PathBuf)> = entries
        .iter()
        .filter_map(|(name, path)| Some((name.as_str(), std::fs::canonicalize(path).ok()?)))
        .filter(|(_, root)| !klipper_real.starts_with(root))
        .collect();
    let owner_of = |file: &str| {
        let real = std::fs::canonicalize(klipper.join(file)).ok()?;
        roots.iter().find(|(_, root)| real.starts_with(root)).map(|(name, _)| (*name).to_owned())
    };
    let mut links = Links { untracked: untracked.clone(), ..Links::default() };
    for file in &untracked {
        match owner_of(file) {
            Some(name) => links.by_entry.entry(name).or_default().push(file.clone()),
            None => links.unknown.push(file.clone()),
        }
    }
    // A plugin folder file that's no entry's is the user's own business: git ignores the folder.
    for file in in_plugins_dir {
        if let Some(name) = owner_of(&file) {
            links.by_entry.entry(name).or_default().push(file);
        }
    }
    links
}

/// The links in Kalico's `klippy/plugins`, as paths in Klipper's repo.
fn plugin_dir_links(klipper: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(klipper.join(PLUGINS_DIR)) else { return Vec::new() };
    let mut files: Vec<String> = dir
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
        .map(|e| format!("{PLUGINS_DIR}/{}", e.file_name().to_string_lossy()))
        .collect();
    files.sort();
    files
}

/// The files an update from `from` to `to` adds to Klipper's repo.
pub async fn added(klipper: &Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let range = format!("{from}..{to}");
    let text = git::git(klipper, &["diff", "--name-only", "--diff-filter=A", &range], git::COMMAND_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    Ok(text.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
}

/// Why an update can't add these files: each one is already there, untracked. `project` is
/// "Kalico" or "Klipper".
pub fn collisions(project: &str, added: &[String], links: &Links) -> Vec<String> {
    added
        .iter()
        .filter(|f| links.untracked.contains(f))
        .map(|f| match links.owner(f) {
            Some(entry) => {
                format!("{project}'s update adds {f}, which is the {entry} entry's link. Remove the link, then update again.")
            }
            None => format!(
                "{project}'s update adds {f}, which is already there and isn't {project}'s. Move it away, then update again."
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    async fn run(dir: &Path, args: &[&str]) {
        git::git(dir, args, git::COMMAND_TIMEOUT).await.unwrap();
    }

    /// A Klipper repo with Kalico's plugin folder, and two plugin repos.
    async fn setup() -> (tempfile::TempDir, PathBuf, Vec<(String, PathBuf)>) {
        let dir = tempfile::tempdir().unwrap();
        let klipper = dir.path().join("klipper");
        std::fs::create_dir_all(klipper.join("klippy/extras")).unwrap();
        std::fs::create_dir_all(klipper.join(PLUGINS_DIR)).unwrap();
        std::fs::write(klipper.join(".gitignore"), "klippy/plugins/*\n").unwrap();
        std::fs::write(klipper.join("klippy/extras/heater.py"), "").unwrap();
        run(&klipper, &["init", "-q", "-b", "main"]).await;
        run(&klipper, &["add", "."]).await;
        run(&klipper, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "k"]).await;
        let led = dir.path().join("led_effect");
        let speed = dir.path().join("auto_speed");
        for (repo, file) in [(&led, "src/led_effect.py"), (&speed, "autospeed/main.py"), (&speed, "auto_speed.py")] {
            std::fs::create_dir_all(repo.join(file).parent().unwrap()).unwrap();
            std::fs::write(repo.join(file), "").unwrap();
        }
        symlink(led.join("src/led_effect.py"), klipper.join("klippy/extras/led_effect.py")).unwrap();
        symlink(speed.join("auto_speed.py"), klipper.join("klippy/extras/auto_speed.py")).unwrap();
        std::fs::create_dir(klipper.join("klippy/extras/autospeed")).unwrap();
        symlink(speed.join("autospeed/main.py"), klipper.join("klippy/extras/autospeed/main.py")).unwrap();
        // A copy nobody updates, and a link in Kalico's folder.
        std::fs::write(klipper.join("klippy/extras/auto_tap.py"), "").unwrap();
        symlink(led.join("src/led_effect.py"), klipper.join("klippy/plugins/led_effect_too.py")).unwrap();
        // An entry for the whole home folder is no plugin's.
        let entries = vec![
            ("home".to_owned(), dir.path().to_path_buf()),
            ("led_effect".to_owned(), led),
            ("klipper_auto_speed".to_owned(), speed),
        ];
        (dir, klipper, entries)
    }

    #[tokio::test]
    async fn sorts_plugin_files_by_entry() {
        let (_dir, klipper, entries) = setup().await;
        let links = find(&klipper, &entries).await;
        assert_eq!(
            links.by_entry["klipper_auto_speed"],
            ["klippy/extras/auto_speed.py", "klippy/extras/autospeed/main.py"]
        );
        assert_eq!(links.by_entry["led_effect"], ["klippy/extras/led_effect.py", "klippy/plugins/led_effect_too.py"]);
        assert_eq!(links.unknown, ["klippy/extras/auto_tap.py"]);
        assert_eq!(links.owner("klippy/extras/led_effect.py"), Some("led_effect"));
        assert!(!links.by_entry.contains_key("home"));
    }

    #[tokio::test]
    async fn finds_collisions_before_git_does() {
        let (_dir, klipper, entries) = setup().await;
        let base = git::git(&klipper, &["rev-parse", "HEAD"], git::COMMAND_TIMEOUT).await.unwrap().trim().to_owned();
        // Upstream adds led_effect.py and auto_tap.py, in a clone, and Klipper's repo fetches it.
        let up = tempfile::tempdir().unwrap();
        let clone = up.path().join("up");
        run(up.path(), &["clone", "-q", klipper.to_str().unwrap(), "up"]).await;
        for f in ["klippy/extras/led_effect.py", "klippy/extras/auto_tap.py", "klippy/extras/new.py"] {
            std::fs::write(clone.join(f), "upstream").unwrap();
        }
        run(&clone, &["add", "klippy"]).await;
        run(&clone, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "up"]).await;
        run(&klipper, &["fetch", "-q", clone.to_str().unwrap(), "main:refs/remotes/origin/main"]).await;
        let to = "origin/main";

        let links = find(&klipper, &entries).await;
        let added = added(&klipper, &base, to).await.unwrap();
        assert_eq!(added, ["klippy/extras/auto_tap.py", "klippy/extras/led_effect.py", "klippy/extras/new.py"]);
        assert_eq!(
            collisions("Kalico", &added, &links),
            [
                "Kalico's update adds klippy/extras/auto_tap.py, which is already there and isn't Kalico's. Move it away, then update again.",
                "Kalico's update adds klippy/extras/led_effect.py, which is the led_effect entry's link. Remove the link, then update again.",
            ]
        );
    }
}
