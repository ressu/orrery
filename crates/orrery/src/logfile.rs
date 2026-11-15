//! orrery's log file, after Moonraker's `moonraker.log`: a new file starts each midnight and the
//! two before it are kept, named `<file>.<YYYY-MM-DD>`. Midnight is UTC here, matching the log
//! timestamps; Moonraker uses local time.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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

fn open_append(path: &Path) -> io::Result<File> {
    File::options().create(true).append(true).open(path)
}

/// Moves the log to `<file>.<date of day>` and removes all but the newest backups.
fn rotate(path: &Path, day: u64) -> io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::rename(path, dir.join(format!("{name}.{}", format_date(day))))?;
    let prefix = format!("{name}.");
    let mut backups: Vec<String> = fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.strip_prefix(&prefix).is_some_and(is_date))
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(BACKUPS);
    for old in &backups[..excess] {
        let _ = fs::remove_file(dir.join(old));
    }
    Ok(())
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
