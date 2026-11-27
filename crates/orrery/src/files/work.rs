//! Work folders, where uploads land and where tools write what they make
//! (docs/design/2026-11-27-file-tools.md). They're in the data folder, on the same filesystem
//! as the gcodes folder as a rule, so a file moves into place with a rename, and a tool's input
//! is a hard link. `/tmp` is often in memory on a printer computer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The folder in the data folder that holds every work folder.
const WORK_DIR: &str = ".orrery-work";

/// The nice value tools run at, so they don't take the CPU from Klipper.
pub const TOOL_NICE: i32 = 19;

static COUNT: AtomicU64 = AtomicU64::new(0);

pub fn root(data: &Path) -> PathBuf {
    data.join(WORK_DIR)
}

/// A name in the work root, for a file or folder no one else uses.
pub fn unique(data: &Path, what: &str) -> PathBuf {
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    root(data).join(format!("{what}-{}-{n}", std::process::id()))
}

/// Removes what an earlier run left, such as after a crash. Each name holds the process id
/// that made it, so a second Orrery on the same data folder keeps its own.
pub fn clean(data: &Path) {
    let dir = root(data);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let pid = name.split('-').nth(1).and_then(|p| p.parse::<u32>().ok());
            if pid.is_some_and(|p| Path::new(&format!("/proc/{p}")).exists()) {
                continue;
            }
            let path = entry.path();
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
    let _ = std::fs::create_dir_all(&dir);
}

/// One tool's work folder, removed when it's dropped.
pub struct WorkDir {
    pub dir: PathBuf,
}

impl WorkDir {
    pub fn new(data: &Path) -> std::io::Result<Self> {
        let dir = unique(data, "tool");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Puts `file` in the folder as `input<ext>`: a hard link when it can, a copy when it
    /// can't, such as across filesystems.
    pub fn link_input(&self, file: &Path) -> std::io::Result<PathBuf> {
        let ext = file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let input = self.dir.join(format!("input{ext}"));
        if std::fs::hard_link(file, &input).is_err() {
            std::fs::copy(file, &input)?;
        }
        Ok(input)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs `f` on a thread of its own at a low priority, which Linux allows a thread. Klipper's
/// process isn't touched.
pub async fn low_priority<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name("orrery-reader".into()).spawn(move || {
        let _ = rustix::process::setpriority_process(Some(rustix::thread::gettid()), TOOL_NICE);
        let _ = tx.send(f());
    });
    if spawned.is_err() {
        return None;
    }
    rx.await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_work_folder_links_its_input_and_goes_away() {
        let data = tempfile::tempdir().unwrap();
        let file = data.path().join("cube.ufp");
        std::fs::write(&file, "zip").unwrap();
        let dir;
        {
            let work = WorkDir::new(data.path()).unwrap();
            dir = work.dir.clone();
            let input = work.link_input(&file).unwrap();
            assert_eq!(input.file_name().unwrap(), "input.ufp");
            assert_eq!(std::fs::read(&input).unwrap(), b"zip");
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(std::fs::metadata(&input).unwrap().ino(), std::fs::metadata(&file).unwrap().ino());
        }
        assert!(!dir.exists());
        // What a process that's gone left is removed; a running one's stays.
        std::fs::write(root(data.path()).join("upload-4194999999-0"), "x").unwrap();
        let mine = unique(data.path(), "upload");
        std::fs::write(&mine, "x").unwrap();
        clean(data.path());
        let left: Vec<_> = std::fs::read_dir(root(data.path())).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(left, [mine]);
    }

    #[tokio::test]
    async fn low_priority_work_runs_niced() {
        let nice = low_priority(|| rustix::process::getpriority_process(Some(rustix::thread::gettid())).unwrap()).await;
        assert_eq!(nice, Some(TOOL_NICE));
    }
}
