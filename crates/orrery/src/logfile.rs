//! orrery's log file, after Moonraker's `moonraker.log`: a new file starts each midnight and the
//! two before it are kept, named `<file>.<YYYY-MM-DD>`. Midnight is UTC here, matching the log
//! timestamps; Moonraker uses local time. `server.logs.rollover` starts a new file on request,
//! and the old one is named `<file>.<YYYY-MM-DD_HH-MM-SS>`.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many earlier files to keep, as Moonraker's `backupCount=2`.
const BACKUPS: usize = 2;
const DAY: u64 = 24 * 60 * 60;

pub struct LogFile {
    path: PathBuf,
    header: String,
    file: File,
    day: u64,
}

impl LogFile {
    /// Opens the log for appending and writes `header` under a "Log Start" line. A file left from
    /// an earlier day is moved aside first.
    pub fn open(path: &Path, header: String) -> io::Result<Self> {
        let today = day_of(SystemTime::now());
        if let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) {
            let day = day_of(modified);
            if day < today {
                rotate(path, day)?;
            }
        }
        let mut log = Self { path: path.to_path_buf(), header, file: open_append(path)?, day: today };
        log.write_header()?;
        Ok(log)
    }

    fn write_header(&mut self) -> io::Result<()> {
        let line = "-".repeat(20);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        writeln!(self.file, "{line} Log Start | {} {line}", format_time(now))?;
        writeln!(self.file, "{}", self.header)
    }

    /// Starts a new file now, as `server.logs.rollover` asks.
    pub fn rollover(&mut self) -> io::Result<()> {
        self.file.flush()?;
        rotate_to(&self.path, &stamp_now())?;
        self.file = open_append(&self.path)?;
        self.day = day_of(SystemTime::now());
        self.write_header()
    }

    /// Makes this the log that [`rollover`] starts again, and returns a writer for it.
    pub fn share(self) -> SharedLog {
        let log = Arc::new(Mutex::new(self));
        let _ = SHARED.set(log.clone());
        SharedLog(log)
    }

    fn rotate_if_needed(&mut self) -> io::Result<()> {
        let today = day_of(SystemTime::now());
        if today == self.day {
            return Ok(());
        }
        self.file.flush()?;
        rotate(&self.path, self.day)?;
        self.file = open_append(&self.path)?;
        self.day = today;
        self.write_header()
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed()?;
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

static SHARED: OnceLock<Arc<Mutex<LogFile>>> = OnceLock::new();

/// orrery's log, written by the logging thread and started again by [`rollover`].
pub struct SharedLog(Arc<Mutex<LogFile>>);

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// The time now as `YYYY-MM-DD_HH-MM-SS`, in UTC, as a rolled-over log's suffix.
pub fn stamp_now() -> String {
    format_stamp(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
}

/// Starts a new log file now. `None` when orrery writes no log file.
pub fn rollover() -> Option<io::Result<()>> {
    SHARED.get().map(|log| log.lock().unwrap_or_else(|e| e.into_inner()).rollover())
}

fn open_append(path: &Path) -> io::Result<File> {
    File::options().create(true).append(true).open(path)
}

/// Moves the log to `<file>.<date of day>` and removes all but the newest backups.
fn rotate(path: &Path, day: u64) -> io::Result<()> {
    rotate_to(path, &format_date(day))
}

/// Moves the log to `<file>.<suffix>` and removes all but the newest backups. The newest are
/// the last written: a rollover on request leaves a backup named for the time, and the
/// midnight one after it is named for the day, which sorts before it.
fn rotate_to(path: &Path, suffix: &str) -> io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::rename(path, dir.join(format!("{name}.{suffix}")))?;
    let prefix = format!("{name}.");
    let mut backups: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_name().to_str().and_then(|n| n.strip_prefix(&prefix)).is_some_and(is_backup))
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH), e.path()))
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(BACKUPS);
    for (_, old) in &backups[..excess] {
        let _ = fs::remove_file(old);
    }
    Ok(())
}

/// Whether `s` is a backup's suffix: `YYYY-MM-DD`, or `YYYY-MM-DD_HH-MM-SS`.
fn is_backup(s: &str) -> bool {
    match s.split_once('_') {
        None => is_date(s),
        Some((date, time)) => {
            is_date(date)
                && time.len() == 8
                && time.bytes().enumerate().all(|(i, b)| if i == 2 || i == 5 { b == b'-' } else { b.is_ascii_digit() })
        }
    }
}

fn day_of(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / DAY
}

/// Whether `s` looks like `YYYY-MM-DD`.
fn is_date(s: &str) -> bool {
    s.len() == 10 && s.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
}

/// The civil date of a day count since 1970-01-01, after Howard Hinnant's `civil_from_days`.
pub fn civil(day: u64) -> (i64, u64, u64) {
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe as i64 + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn format_date(day: u64) -> String {
    let (y, m, d) = civil(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD_HH-MM-SS`, as Moonraker names a Klipper log it rolls over.
fn format_stamp(secs: u64) -> String {
    let rest = secs % DAY;
    format!("{}_{:02}-{:02}-{:02}", format_date(secs / DAY), rest / 3600, rest % 3600 / 60, rest % 60)
}

fn format_time(secs: u64) -> String {
    let rest = secs % DAY;
    format!("{} {:02}:{:02}:{:02} UTC", format_date(secs / DAY), rest / 3600, rest % 3600 / 60, rest % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(20_407), "2025-11-15");
        assert_eq!(format_date(11_016), "2000-02-29");
        assert_eq!(format_time(86_399), "1970-01-01 23:59:59 UTC");
        assert!(is_date("2026-11-15"));
        assert!(!is_date("2026-11-1x"));
        assert_eq!(format_stamp(86_399), "1970-01-01_23-59-59");
        assert!(is_backup("2026-11-15_08-30-00") && is_backup("2026-11-15"));
        assert!(!is_backup("2026-11-15_08-30") && !is_backup("keep"));
    }

    #[test]
    fn rollover_starts_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orrery.log");
        let mut log = LogFile::open(&path, "orrery test".into()).unwrap();
        log.write_all(b"before\n").unwrap();
        log.rollover().unwrap();
        log.write_all(b"after\n").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("Log Start") && text.ends_with("orrery test\nafter\n"), "{text}");
        let backup = fs::read_dir(dir.path()).unwrap().flatten().find(|e| e.path() != path).unwrap();
        assert!(fs::read_to_string(backup.path()).unwrap().ends_with("before\n"));
    }

    #[test]
    fn rotates_and_keeps_two() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orrery.log");
        for day in [100, 101, 102] {
            fs::write(&path, "old").unwrap();
            rotate(&path, day).unwrap();
        }
        fs::write(dir.path().join("orrery.log.keep"), "").unwrap();
        let mut names: Vec<String> =
            fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["orrery.log.1970-04-12", "orrery.log.1970-04-13", "orrery.log.keep"]);
        let mut log = LogFile::open(&path, "orrery test".into()).unwrap();
        log.write_all(b"line\n").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("Log Start") && text.contains("orrery test\nline\n"), "{text}");
    }
}
