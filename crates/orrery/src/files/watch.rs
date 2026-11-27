//! The file watcher: it sees changes made to the roots outside orrery's API, such as a file
//! copied in with scp or a `printer.cfg` that Klipper saves, and sends `notify_filelist_changed`
//! for them, as Moonraker's inotify observer does. New gcode files get their metadata first.
//!
//! How it differs from Moonraker's observer:
//! - It doesn't ask the kernel for each write, only for "closed after writing", so it doesn't
//!   wake for every line Klipper adds to its log. A file is announced once it's closed.
//! - Changes wait until the roots have been quiet for a moment, and are then announced
//!   together. A file created and removed in that time, such as a lock or temporary file, isn't
//!   announced. A file written and then renamed into place is announced once, at its new name.
//!   The files of a new folder are part of its `create_dir`, as in Moonraker.
//! - The API announces its own changes at once, and marks their paths for the watcher to leave
//!   out. Moonraker instead holds the API's notification back for a second, and syncs the two
//!   with a lock.
//! - If the kernel's event queue overflows, orrery watches the roots again and sends
//!   `root_update`, so clients list them again.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use super::{FileManager, after_move, changed_event, clear_metadata, parse_metadata, scan_directory_metadata};
use crate::server::App;

/// How long the roots must be quiet before the changes are announced.
const QUIET: Duration = Duration::from_millis(250);
/// A steady stream of changes is still announced this often.
const MAX_WAIT: Duration = Duration::from_secs(2);
/// How long after an API request ends the watcher still leaves its paths out.
const OWN_CHANGE_GRACE: Duration = Duration::from_secs(2);

const WATCH_FLAGS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::ONLYDIR);

/// A path the API is changing. `until` is set when the request ends.
pub(super) struct OwnChange {
    id: u64,
    path: PathBuf,
    until: Option<Instant>,
}

/// Keeps paths marked as orrery's own changes; see [`FileManager::own_change`].
pub struct OwnChangeGuard<'a> {
    fm: &'a FileManager,
    id: u64,
}

impl FileManager {
    /// Marks `paths`, and everything below them, as changed by orrery itself, so the watcher
    /// doesn't announce them a second time. The mark lasts until the guard is dropped, and
    /// [`OWN_CHANGE_GRACE`] after that, for the events still on their way.
    pub fn own_change(&self, paths: &[&Path]) -> OwnChangeGuard<'_> {
        let id = self.next_own_change.fetch_add(1, Ordering::Relaxed);
        let mut own = self.own_changes.lock().expect("own changes lock");
        own.extend(paths.iter().map(|p| OwnChange { id, path: p.to_path_buf(), until: None }));
        OwnChangeGuard { fm: self, id }
    }

    fn is_own_change(&self, path: &Path) -> bool {
        let now = Instant::now();
        let mut own = self.own_changes.lock().expect("own changes lock");
        own.retain(|c| c.until.is_none_or(|t| t > now));
        own.iter().any(|c| path.starts_with(&c.path))
    }
}

impl Drop for OwnChangeGuard<'_> {
    fn drop(&mut self) {
        let until = Instant::now() + OWN_CHANGE_GRACE;
        let mut own = self.fm.own_changes.lock().expect("own changes lock");
        for c in own.iter_mut().filter(|c| c.id == self.id) {
            c.until = Some(until);
        }
    }
}

/// A change waiting to be announced.
#[derive(Debug, PartialEq)]
enum Change {
    /// A file closed after writing: `create_file` or `modify_file`.
    File {
        root: String,
        path: PathBuf,
        action: &'static str,
    },
    /// A new folder.
    Dir {
        root: String,
        path: PathBuf,
    },
    Deleted {
        root: String,
        path: PathBuf,
        is_dir: bool,
    },
    Moved {
        src_root: String,
        src: PathBuf,
        root: String,
        path: PathBuf,
        is_dir: bool,
    },
    /// Moved out of a watched folder: a move within the roots, if the other half comes, and
    /// otherwise a delete.
    Away {
        cookie: u32,
        root: String,
        path: PathBuf,
        is_dir: bool,
    },
    RootUpdate {
        root: String,
        path: PathBuf,
    },
}

impl Change {
    fn path(&self) -> &Path {
        match self {
            Change::File { path, .. }
            | Change::Dir { path, .. }
            | Change::Deleted { path, .. }
            | Change::Moved { path, .. }
            | Change::Away { path, .. }
            | Change::RootUpdate { path, .. } => path,
        }
    }

    /// A file or folder that's new since the last announcement.
    fn is_new(&self) -> bool {
        matches!(self, Change::File { action: "create_file", .. } | Change::Dir { .. })
    }
}

/// An inotify event, copied out of the read buffer.
struct Event {
    wd: i32,
    mask: ReadFlags,
    cookie: u32,
    name: Option<OsString>,
}

struct Dir {
    root: String,
    path: PathBuf,
}

struct Watcher {
    app: Arc<App>,
    fd: AsyncFd<OwnedFd>,
    dirs: HashMap<i32, Dir>,
    /// Files created and not closed yet.
    open: HashSet<PathBuf>,
    changes: Vec<Change>,
    first: Option<Instant>,
    last: Instant,
    warned: bool,
    announce: mpsc::UnboundedSender<Change>,
}

/// Starts watching the roots, unless `file_system_observer` turns it off.
pub fn start(app: &Arc<App>) {
    match app.files.observer.as_str() {
        "inotify" => {}
        "none" => {
            tracing::info!("File watching is off: file_system_observer is none");
            return;
        }
        other => {
            app.add_warning(format!(
                "[file_manager]: Invalid value '{other}' for option 'file_system_observer'.  Falling back to no observer."
            ));
            return;
        }
    }
    let fd = match inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK) {
        // SAFETY: the AsyncFd owns the descriptor, and nothing else closes or replaces it.
        Ok(fd) => {
            unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }.map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    };
    let fd = match fd {
        Ok(fd) => fd,
        Err(e) => {
            app.add_warning(format!("orrery can't watch the files for changes: {e}"));
            return;
        }
    };
    let (announce, rx) = mpsc::unbounded_channel();
    tokio::spawn(announce_changes(app.clone(), rx));
    let mut watcher = Watcher {
        app: app.clone(),
        fd,
        dirs: HashMap::new(),
        open: HashSet::new(),
        changes: Vec::new(),
        first: None,
        last: Instant::now(),
        warned: false,
        announce,
    };
    watcher.watch_roots();
    tracing::info!("Watching {} folders for changes", watcher.dirs.len());
    tokio::spawn(watcher.run());
}

impl Watcher {
    async fn run(mut self) {
        let mut buf = vec![MaybeUninit::<u8>::uninit(); 16 * 1024];
        loop {
            let deadline = self.deadline();
            let events = tokio::select! {
                ready = self.fd.readable() => {
                    let mut ready = match ready {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("File watching stopped: {e}");
                            return;
                        }
                    };
                    match read_events(ready.get_inner(), &mut buf) {
                        Ok(events) => {
                            ready.clear_ready();
                            events
                        }
                        Err(e) => {
                            tracing::warn!("File watching stopped: {e}");
                            return;
                        }
                    }
                }
                () = sleep_until(deadline), if deadline.is_some() => {
                    self.flush();
                    continue;
                }
            };
            for event in events {
                self.handle(event);
            }
        }
    }

    fn deadline(&self) -> Option<Instant> {
        let first = self.first?;
        Some((self.last + QUIET).min(first + MAX_WAIT))
    }

    fn watch_roots(&mut self) {
        let roots: Vec<(String, PathBuf)> =
            self.app.files.roots.iter().map(|r| (r.name.clone(), r.path.clone())).collect();
        for (root, path) in roots {
            self.watch_tree(&root, &path);
        }
    }

    /// Watches `top` and the folders below it. Each folder is watched before it's listed, so a
    /// folder made inside it meanwhile is either listed or reported.
    fn watch_tree(&mut self, root: &str, top: &Path) {
        let mut stack = vec![top.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if !self.add_watch(root, &dir) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && !self.ignored(&path) {
                    stack.push(path);
                }
            }
        }
    }

    /// Whether to watch the folder: no `.git` folders, and no reserved paths, as in Moonraker.
    fn ignored(&self, path: &Path) -> bool {
        self.app.files.is_reserved(path, true).is_some()
    }

    /// Returns whether the folder is watched now under this path. The kernel gives a folder
    /// that's already watched its old number, so a symlink to a watched folder, or a loop of
    /// symlinks, isn't watched twice.
    fn add_watch(&mut self, root: &str, dir: &Path) -> bool {
        match inotify::add_watch(self.fd.get_ref(), dir, WATCH_FLAGS) {
            Ok(wd) => match self.dirs.get(&wd) {
                Some(known) if known.path != dir => {
                    tracing::debug!(
                        "{} is the folder {}, which is watched already",
                        dir.display(),
                        known.path.display()
                    );
                    false
                }
                Some(_) => true,
                None => {
                    self.dirs.insert(wd, Dir { root: root.to_owned(), path: dir.to_path_buf() });
                    true
                }
            },
            Err(e) => {
                let message = if e == Errno::NOSPC {
                    format!(
                        "orrery can't watch every folder for changes: the system's limit is reached at {}. \
                         Raise fs.inotify.max_user_watches to watch more.",
                        dir.display()
                    )
                } else {
                    format!("orrery can't watch {} for changes: {e}", dir.display())
                };
                if !self.warned && self.app.files.observer_warnings {
                    self.app.add_warning(message);
                    self.warned = true;
                } else {
                    tracing::info!("{message}");
                }
                false
            }
        }
    }

    /// Stops watching `top` and the folders below it.
    fn unwatch(&mut self, top: &Path) {
        let gone: Vec<i32> = self.dirs.iter().filter(|(_, d)| d.path.starts_with(top)).map(|(wd, _)| *wd).collect();
        for wd in gone {
            let _ = inotify::remove_watch(self.fd.get_ref(), wd);
            self.dirs.remove(&wd);
        }
    }

    fn handle(&mut self, event: Event) {
        if event.mask.contains(ReadFlags::QUEUE_OVERFLOW) {
            self.overflow();
            return;
        }
        if event.mask.contains(ReadFlags::IGNORED) {
            // The folder was removed, or orrery stopped watching it.
            self.dirs.remove(&event.wd);
            return;
        }
        let (Some(dir), Some(name)) = (self.dirs.get(&event.wd), event.name) else { return };
        let root = dir.root.clone();
        let path = dir.path.join(name);
        let is_dir = event.mask.contains(ReadFlags::ISDIR);
        let now = Instant::now();
        self.last = now;
        self.first.get_or_insert(now);
        if event.mask.contains(ReadFlags::CREATE) {
            if is_dir {
                self.new_dir(root, path);
            } else if path.is_symlink() {
                // A symlink is complete when it's made; it's never written.
                self.push_file(root, path, "create_file");
            } else {
                self.open.insert(path);
            }
        } else if event.mask.contains(ReadFlags::CLOSE_WRITE) {
            let action = if self.open.remove(&path) { "create_file" } else { "modify_file" };
            self.push_file(root, path, action);
        } else if event.mask.contains(ReadFlags::DELETE) {
            self.open.remove(&path);
            self.changes.push(Change::Deleted { root, path, is_dir });
        } else if event.mask.contains(ReadFlags::MOVED_FROM) {
            self.changes.push(Change::Away { cookie: event.cookie, root, path, is_dir });
        } else if event.mask.contains(ReadFlags::MOVED_TO) {
            let away =
                self.changes.iter().position(|c| matches!(c, Change::Away { cookie, .. } if *cookie == event.cookie));
            match away.map(|i| self.changes.remove(i)) {
                Some(Change::Away { root: src_root, path: src, .. }) => self.moved(src_root, src, root, path, is_dir),
                // Moved in from outside the roots.
                _ if is_dir => self.new_dir(root, path),
                _ => self.push_file(root, path, "create_file"),
            }
        }
    }

    fn new_dir(&mut self, root: String, path: PathBuf) {
        if self.ignored(&path) {
            return;
        }
        self.watch_tree(&root, &path);
        self.changes.push(Change::Dir { root, path });
    }

    /// A file closed after writing. One already waiting keeps its action, so a new file that's
    /// written twice is still `create_file`.
    fn push_file(&mut self, root: String, path: PathBuf, action: &'static str) {
        let waiting = self.changes.iter().any(|c| matches!(c, Change::File { path: p, .. } if *p == path));
        if !waiting {
            self.changes.push(Change::File { root, path, action });
        }
    }

    fn moved(&mut self, src_root: String, src: PathBuf, root: String, path: PathBuf, is_dir: bool) {
        if is_dir {
            if self.ignored(&path) {
                // Renamed to `.git` or a reserved path: gone, for clients.
                self.unwatch(&src);
                self.changes.push(Change::Deleted { root: src_root, path: src, is_dir });
                return;
            }
            for d in self.dirs.values_mut() {
                if let Some(moved) = rebase(&d.path, &src, &path) {
                    d.path = moved;
                    d.root.clone_from(&root);
                }
            }
        }
        // Moved while still being written: it's announced at its new name when it's closed.
        if self.open.remove(&src) {
            self.open.insert(path);
            return;
        }
        // Written, then renamed into place before it was announced: announce it at its new
        // name. The same goes for a new folder that was renamed.
        let mut renamed = false;
        for c in &mut self.changes {
            match c {
                Change::File { root: r, path: p, .. } | Change::Dir { root: r, path: p } => {
                    if let Some(moved) = rebase(p, &src, &path) {
                        renamed |= *p == src;
                        *p = moved;
                        r.clone_from(&root);
                    }
                }
                _ => {}
            }
        }
        if !renamed {
            self.changes.push(Change::Moved { src_root, src, root, path, is_dir });
        }
    }

    /// Too many changes at once for the kernel's queue: watch everything again, and have
    /// clients list the roots again.
    fn overflow(&mut self) {
        tracing::warn!("Too many file changes at once to follow one by one, so orrery checks every folder again");
        let wds: Vec<i32> = self.dirs.keys().copied().collect();
        for wd in wds {
            let _ = inotify::remove_watch(self.fd.get_ref(), wd);
        }
        self.dirs.clear();
        self.open.clear();
        self.changes.clear();
        self.watch_roots();
        for r in &self.app.files.roots {
            self.changes.push(Change::RootUpdate { root: r.name.clone(), path: r.path.clone() });
        }
        self.first.get_or_insert(Instant::now());
    }

    /// Announces the waiting changes.
    fn flush(&mut self) {
        self.first = None;
        let mut changes = std::mem::take(&mut self.changes);
        for c in &mut changes {
            // The other half of a move never came, so it left the roots.
            if let Change::Away { root, path, is_dir, .. } = c {
                if *is_dir {
                    let p = path.clone();
                    self.unwatch(&p);
                }
                self.open.remove(path.as_path());
                *c = Change::Deleted { root: std::mem::take(root), path: std::mem::take(path), is_dir: *is_dir };
            }
        }
        for c in coalesce(changes) {
            let own = match &c {
                Change::Moved { src, path, .. } => {
                    self.app.files.is_own_change(src) || self.app.files.is_own_change(path)
                }
                Change::RootUpdate { .. } => false,
                // What no client can read, such as the gcode cache, isn't listed, so it isn't
                // announced either.
                c => self.app.files.is_own_change(c.path()) || self.app.files.is_reserved(c.path(), false).is_some(),
            };
            if !own {
                let _ = self.announce.send(c);
            }
        }
    }
}

/// `path` moved from below `from` to below `to`, or `None` if it isn't below `from`.
fn rebase(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(from).ok()?;
    // Joining an empty path would add a trailing slash.
    Some(if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) })
}

/// Leaves out what clients don't need to hear about: a file or folder created and removed
/// again, changes inside a new folder (its `create_dir` covers them), and removals inside a
/// removed folder.
fn coalesce(changes: Vec<Change>) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    for c in changes {
        if let Change::Deleted { path, .. } = &c {
            let was_new = out.iter().any(|o| o.is_new() && o.path() == path);
            out.retain(|o| !(matches!(o, Change::File { .. } | Change::Dir { .. }) && o.path().starts_with(path)));
            if was_new {
                continue;
            }
        }
        out.push(c);
    }
    let new_dirs: Vec<PathBuf> =
        out.iter().filter(|c| matches!(c, Change::Dir { .. })).map(|c| c.path().to_path_buf()).collect();
    let deleted_dirs: Vec<PathBuf> = out
        .iter()
        .filter(|c| matches!(c, Change::Deleted { is_dir: true, .. }))
        .map(|c| c.path().to_path_buf())
        .collect();
    let inside = |dirs: &[PathBuf], p: &Path| dirs.iter().any(|d| p != d && p.starts_with(d));
    out.retain(|c| match c {
        Change::File { path, .. } | Change::Dir { path, .. } => !inside(&new_dirs, path),
        Change::Deleted { path, .. } => !inside(&deleted_dirs, path),
        _ => true,
    });
    out
}

/// Reads every event the kernel has ready.
fn read_events(fd: &OwnedFd, buf: &mut [MaybeUninit<u8>]) -> std::io::Result<Vec<Event>> {
    let mut reader = inotify::Reader::new(fd, buf);
    let mut events = Vec::new();
    loop {
        match reader.next() {
            Ok(e) => events.push(Event {
                wd: e.wd(),
                mask: e.events(),
                cookie: e.cookie(),
                name: e.file_name().map(|n| std::ffi::OsStr::from_bytes(n.to_bytes()).to_owned()),
            }),
            Err(Errno::AGAIN) => return Ok(events),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    if let Some(d) = deadline {
        tokio::time::sleep_until(d.into()).await;
    }
}

/// Announces changes one at a time, in order. A new gcode file gets its metadata first, so a
/// client that lists it sees its print time and thumbnails.
async fn announce_changes(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<Change>) {
    while let Some(change) = rx.recv().await {
        let (action, root, path) = match change {
            Change::File { root, path, action } => {
                if !path.is_file() {
                    continue;
                }
                if root == "gcodes" {
                    parse_metadata(&app, &path).await;
                }
                changed_event(&app, action, &root, &path, None);
                (action, root, path)
            }
            Change::Dir { root, path } => {
                if !path.is_dir() {
                    continue;
                }
                if root == "gcodes" {
                    scan_directory_metadata(&app, &path).await;
                }
                changed_event(&app, "create_dir", &root, &path, None);
                ("create_dir", root, path)
            }
            Change::Deleted { root, path, is_dir } => {
                clear_metadata(&app, &root, &path, is_dir);
                let action = if is_dir { "delete_dir" } else { "delete_file" };
                changed_event(&app, action, &root, &path, None);
                (action, root, path)
            }
            Change::Moved { src_root, src, root, path, is_dir } => {
                if !path.exists() {
                    continue;
                }
                after_move(&app, &src_root, &root, &src, &path).await;
                let action = if is_dir { "move_dir" } else { "move_file" };
                changed_event(&app, action, &root, &path, Some((&src_root, &src)));
                (action, root, path)
            }
            Change::RootUpdate { root, path } => {
                if root == "gcodes" {
                    scan_directory_metadata(&app, &path).await;
                }
                changed_event(&app, "root_update", &root, &path, None);
                ("root_update", root, path)
            }
            Change::Away { .. } => continue,
        };
        // Other programs change files all day, and logs all the time, so these lines would
        // mostly wear the SD card: `-v` shows them, and RUST_LOG=trace the logs folder's too.
        let rel = app.files.relative_path(&root, &path);
        if root == "logs" {
            tracing::trace!("Changed outside orrery: {action} {root}/{rel}");
        } else {
            tracing::debug!("Changed outside orrery: {action} {root}/{rel}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, action: &'static str) -> Change {
        Change::File { root: "gcodes".into(), path: path.into(), action }
    }

    fn dir(path: &str) -> Change {
        Change::Dir { root: "gcodes".into(), path: path.into() }
    }

    fn deleted(path: &str, is_dir: bool) -> Change {
        Change::Deleted { root: "gcodes".into(), path: path.into(), is_dir }
    }

    #[test]
    fn rebases() {
        let (from, to) = (Path::new("/c/.a.tmp"), Path::new("/c/a"));
        assert_eq!(rebase(from, from, to).unwrap().as_os_str(), "/c/a");
        assert_eq!(rebase(Path::new("/d/x/y"), Path::new("/d/x"), Path::new("/e")).unwrap(), Path::new("/e/y"));
        assert_eq!(rebase(Path::new("/d/xy"), Path::new("/d/x"), Path::new("/e")), None);
    }

    #[test]
    fn coalesces() {
        // A temporary file comes and goes; an edited file is then removed.
        let out = coalesce(vec![
            file("/g/tmp", "create_file"),
            file("/g/a", "modify_file"),
            deleted("/g/tmp", false),
            deleted("/g/a", false),
        ]);
        assert_eq!(out, [deleted("/g/a", false)]);
        // A copied folder is one create_dir; a removed tree is one delete_dir.
        let out = coalesce(vec![
            dir("/g/d"),
            dir("/g/d/e"),
            file("/g/d/e/x", "create_file"),
            deleted("/g/old/x", false),
            deleted("/g/old/sub", true),
            deleted("/g/old", true),
        ]);
        assert_eq!(out, [dir("/g/d"), deleted("/g/old", true)]);
    }
}
