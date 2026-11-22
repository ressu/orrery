//! `orrery preflight`: the check an update runs from the new orrery before it takes effect
//! (docs/design/2026-11-22-self-update.md, "An update"). It starts nothing, opens no port,
//! writes nothing, and checks the minimum a new orrery needs to start:
//! - it runs on this CPU, since it prints anything at all;
//! - the config files parse;
//! - this version reads the database.
//!
//! The agents come from the same archive, checked against its checksum, so it doesn't run
//! them; CI's smoke test runs each program of a release once. An agent of another version, as
//! after a copy by hand, is a warning when it connects.

use std::path::Path;

use crate::config::Config;
use crate::database::{self, Readable};
use crate::server::Paths;

/// What the checks found: one line each, and whether they all passed.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    pub ok: bool,
}

impl Report {
    fn pass(&mut self, line: String) {
        self.lines.push(line);
    }

    fn fail(&mut self, line: String) {
        self.lines.push(line);
        self.ok = false;
    }
}

/// Runs the checks with orrery's config and data folder.
pub fn run(paths: &Paths, toml: (&Path, bool)) -> Report {
    let mut report = Report { ok: true, ..Report::default() };
    report.pass(format!("orrery {} runs on this CPU.", orrery_build::LONG_VERSION));
    let Some(config) = config(&mut report, paths, toml) else { return report };
    let file = database::file(&config, paths);
    match database::check_readable(&file) {
        Ok(Readable::Yes) => report.pass(format!("This version reads the database {}.", file.display())),
        Ok(Readable::Missing) => {
            report.pass(format!("There's no database at {} yet. orrery makes one when it starts.", file.display()))
        }
        Ok(Readable::InUse) => report.pass(format!(
            "The running orrery has the database {} open, so orrery checks it when it starts.",
            file.display()
        )),
        Err(e) => report.fail(format!("This version can't read the database: {e:#}")),
    }
    report
}

/// The config files parse, as orrery reads them when it starts.
fn config(report: &mut Report, paths: &Paths, (toml, required): (&Path, bool)) -> Option<Config> {
    let mut config = match Config::load(&paths.config_file) {
        Ok(c) => c,
        Err(e) => {
            report.fail(format!("The config doesn't parse: {e:#}"));
            return None;
        }
    };
    let mut files = vec![paths.config_file.display().to_string()];
    if required || toml.exists() {
        if let Err(e) = config.load_toml(toml) {
            report.fail(format!("orrery.toml doesn't parse: {e:#}"));
            return None;
        }
        files.push(toml.display().to_string());
    }
    let port = config.section("server").get_int("port", 7125);
    if let Err(e) = port {
        report.fail(format!("The config doesn't parse: {e:#}"));
        return None;
    }
    report.pass(format!("The config parses: {}.", files.join(" and ")));
    Some(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path, conf: &str) -> Paths {
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let config_file = dir.join("config/moonraker.conf");
        std::fs::write(&config_file, conf).unwrap();
        Paths { data: dir.to_path_buf(), config_file, log_file: None }
    }

    #[test]
    fn checks_config_and_database() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path(), "[server]\nport: 7125\n");
        let toml = dir.path().join("config/orrery.toml");

        // No database yet: a new orrery makes one. The check writes nothing.
        let r = run(&p, (&toml, false));
        assert!(r.ok, "{:?}", r.lines);
        assert!(r.lines[2].starts_with("There's no database at "), "{:?}", r.lines);
        assert!(!dir.path().join("database").exists());

        // One this version reads, and one the running orrery has open.
        let config = Config::load(&p.config_file).unwrap();
        let db = database::Database::open(&config, &p).unwrap();
        let r = run(&p, (&toml, false));
        assert!(r.ok && r.lines[2].starts_with("The running orrery has the database "), "{:?}", r.lines);
        drop(db);
        let r = run(&p, (&toml, false));
        assert!(r.ok && r.lines[2].starts_with("This version reads the database "), "{:?}", r.lines);

        // A file that isn't a database fails.
        std::fs::write(dir.path().join("database/orrery.redb"), "not a database").unwrap();
        let r = run(&p, (&toml, false));
        assert!(!r.ok && r.lines[2].starts_with("This version can't read the database: "), "{:?}", r.lines);
    }

    #[test]
    fn config_that_doesnt_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path(), "[server]\nport: seven\n");
        let toml = dir.path().join("config/orrery.toml");
        let r = run(&p, (&toml, false));
        assert!(!r.ok, "{:?}", r.lines);
        assert!(r.lines[1].starts_with("The config doesn't parse: "), "{:?}", r.lines);

        let p = paths(dir.path(), "[server]\n");
        std::fs::write(&toml, "server = [").unwrap();
        let r = run(&p, (&toml, false));
        assert!(!r.ok && r.lines[1].starts_with("orrery.toml doesn't parse: "), "{:?}", r.lines);
        // --orrery-config names a file that must be there.
        let r = run(&p, (&dir.path().join("missing.toml"), true));
        assert!(!r.ok && r.lines[1].starts_with("orrery.toml doesn't parse: "), "{:?}", r.lines);
    }
}
