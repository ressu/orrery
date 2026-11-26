//! `sudo orrery setup`: the system files orrery needs to run as a service and to control
//! services (docs/design/2026-11-19-service-control.md, "Deployment").
//!
//! - `orrery-admin`, a system group. Only `orrery.service` gets it, with
//!   `SupplementaryGroups`, so the printer's user's own shells aren't trusted.
//! - `/etc/systemd/system/orrery.service`, which runs orrery as the user who ran sudo.
//! - `/etc/polkit-1/rules.d/orrery.rules`, which lets processes of that user with the group
//!   manage units, reboot, shut down, and update packages. The strict rule allows only orrery's
//!   allowed units.
//!
//! By default setup only writes the files, so it's also how orrery's setup is upgraded.
//! `--disable-moonraker` and `--start` switch over from Moonraker, and `--migrate` does both.
//! With Moonraker compatibility, `moonraker.service` becomes a name for `orrery.service`.
//! `--alias-moonraker` and `--no-alias-moonraker` choose, then `[moonraker_compat]
//! service_alias`, then the last choice, recorded in the unit. Moonraker's own unit file is
//! moved aside only with `--force` or `--migrate`.
//! Every system path is joined to a root folder, and every command goes through [`Host`], so
//! the tests run in a temporary folder.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const GROUP: &str = "orrery-admin";
const UNIT_PATH: &str = "etc/systemd/system/orrery.service";
const RULE_PATH: &str = "etc/polkit-1/rules.d/orrery.rules";
const SERVICE: &str = "orrery.service";
const MOONRAKER: &str = "moonraker.service";
/// Where the alias goes, and where Moonraker's installer puts its unit.
const MOONRAKER_PATH: &str = "etc/systemd/system/moonraker.service";
/// Moonraker's own unit, moved aside for the alias. systemd ignores the name.
const SAVED_PATH: &str = "etc/systemd/system/moonraker.service.orrery-saved";
/// The line in orrery's unit that makes it answer for Moonraker.
const ALIAS_LINE: &str = "Alias=moonraker.service";
/// The line in orrery's unit that records `--no-alias-moonraker`, so later runs keep it.
const NO_ALIAS_LINE: &str = "# moonraker.service isn't a name for this unit: setup ran with --no-alias-moonraker.";
/// The first line of the strict rule, so `--check` knows which rule is installed.
const STRICT_MARK: &str = "// orrery setup: strict rule";

/// The actions orrery needs from logind: power off, reboot, and halt.
const POWER_ACTIONS: &[&str] = &[
    "org.freedesktop.login1.power-off",
    "org.freedesktop.login1.power-off-multiple-sessions",
    "org.freedesktop.login1.reboot",
    "org.freedesktop.login1.reboot-multiple-sessions",
    "org.freedesktop.login1.halt",
    "org.freedesktop.login1.halt-multiple-sessions",
];
const MANAGE_UNITS: &str = "org.freedesktop.systemd1.manage-units";
/// The actions the update manager needs from PackageKit: refresh the package lists, install
/// packages that plugins depend on, and update the system. Moonraker's rule allows every
/// PackageKit action; these three are all that `system_deploy.py:239-245` checks for.
const PACKAGE_ACTIONS: &[&str] = &[
    "org.freedesktop.packagekit.system-sources-refresh",
    "org.freedesktop.packagekit.package-install",
    "org.freedesktop.packagekit.system-update",
];

/// Runs the programs setup needs: `getent`, `groupadd`, `systemctl`, and so on.
pub trait Host {
    /// Runs a program. Returns whether it succeeded, and what it printed. An error means it
    /// couldn't run at all.
    fn run(&mut self, program: &str, args: &[&str]) -> Result<(bool, String)>;

    /// Calls a method on the running orrery, over its unix socket.
    fn ask_orrery(&mut self, socket: &Path, method: &str) -> Result<serde_json::Value> {
        let _ = (socket, method);
        bail!("orrery isn't reachable")
    }
}

/// The real system.
pub struct System;

impl Host for System {
    fn run(&mut self, program: &str, args: &[&str]) -> Result<(bool, String)> {
        let out =
            std::process::Command::new(program).args(args).output().with_context(|| format!("can't run {program}"))?;
        Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    fn ask_orrery(&mut self, socket: &Path, method: &str) -> Result<serde_json::Value> {
        crate::unix_socket::call(socket, method, serde_json::json!({}))
    }
}

#[derive(Debug, Default, Clone)]
pub struct Options {
    /// Report what's in place, and change nothing.
    pub check: bool,
    /// Remove what setup installed.
    pub remove: bool,
    /// Allow only orrery's allowed units, not every unit.
    pub strict: bool,
    /// Stop and disable `moonraker.service`.
    pub disable_moonraker: bool,
    /// Enable `orrery.service`, and start or restart it.
    pub start: bool,
    /// `--alias-moonraker` or `--no-alias-moonraker`: whether `moonraker.service` is a name
    /// for `orrery.service`. `None`: the config or the last choice decides.
    pub alias: Option<bool>,
    /// Move Moonraker's own unit file aside, so `moonraker.service` can be orrery's name.
    pub force: bool,
    /// The server's path options, as given. They go into `ExecStart`.
    pub data_path: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub orrery_config: Option<PathBuf>,
    /// `SUDO_USER`: who orrery runs as.
    pub sudo_user: Option<String>,
    /// Whether setup runs as root.
    pub is_root: bool,
    /// The user id setup runs as, for `--check` without sudo.
    pub uid: u32,
    /// The orrery binary that `ExecStart` runs.
    pub exe: PathBuf,
    /// The folder relative paths are relative to.
    pub cwd: PathBuf,
}

/// What setup did or found, one line each, and whether everything is in place.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    /// `false` when `--check` finds something missing or different.
    pub ok: bool,
}

impl Report {
    fn line(&mut self, text: impl Into<String>) {
        self.lines.push(text.into());
    }

    /// One `--check` finding, with its status in a column of its own.
    fn found(&mut self, status: Status, text: impl AsRef<str>) {
        if status == Status::Update {
            self.ok = false;
        }
        let mut label = status.label();
        for part in wrap(text.as_ref(), CHECK_WIDTH - CHECK_INDENT) {
            self.lines.push(format!("  {label:<7}  {part}"));
            label = "";
        }
    }
}

/// How wide `--check`'s lines are, and where their text starts after the status.
const CHECK_WIDTH: usize = 100;
const CHECK_INDENT: usize = 11;

/// `text` in lines of at most `width` characters, broken at spaces. A longer word, such as a
/// path, gets a line of its own.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split(' ') {
        let line = lines.last_mut().expect("one line");
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(word.to_owned());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

/// What `--check` makes of one part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// In place.
    Ok,
    /// `sudo orrery setup` would change it.
    Update,
    /// The check can't tell.
    Unknown,
    /// Information, nothing to do.
    Note,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Update => "update",
            Self::Unknown => "unknown",
            Self::Note => "note",
        }
    }
}

/// The account orrery runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub home: PathBuf,
}

pub fn run(options: &Options, root: &Path, host: &mut dyn Host) -> Result<Report> {
    if !options.check && !options.is_root {
        bail!("orrery setup changes system files, so it needs root. Run it with sudo:\n  sudo orrery setup");
    }
    let account = account(options, host)?;
    let sys = |path: &str| root.join(path);
    if options.remove {
        return remove(host, &sys);
    }
    check_polkit(host)?;
    let installed_unit = std::fs::read_to_string(sys(UNIT_PATH)).ok();
    let args = server_args(options, &account, installed_unit.as_deref())?;
    let config = load_config(&account, &args)?;
    let moonraker_unit = moonraker_unit(&sys(MOONRAKER_PATH))?;
    // Whether moonraker.service is orrery's name: the flag, then orrery.toml, then the choice
    // recorded in the installed unit, then the default, on with Moonraker compatibility.
    let compat = crate::compat::MoonrakerCompat::new(&config)?;
    let recorded = installed_unit.as_deref().and_then(|u| {
        if u.lines().any(|l| l == ALIAS_LINE) {
            Some(true)
        } else if u.lines().any(|l| l == NO_ALIAS_LINE) {
            Some(false)
        } else {
            None
        }
    });
    let from_toml = config.toml_has("moonraker_compat", "service_alias").then_some(compat.service_alias);
    let want_alias = options.alias.or(from_toml).or(recorded).unwrap_or(compat.service_alias);
    // Off by a flag, now or before: the unit records it. orrery.toml records its own.
    let declined = !want_alias && from_toml.is_none() && options.alias.or(recorded) == Some(false);
    // The command that repeats this run's choice, for the messages.
    let flag = match options.alias {
        Some(true) => " --alias-moonraker",
        Some(false) => " --no-alias-moonraker",
        None => "",
    };
    let blocked = want_alias && moonraker_unit == MoonrakerUnit::Own && !options.force;
    let has_name = moonraker_unit == MoonrakerUnit::Alias;
    // systemd loses track of a running unit whose names change at the reload: it reports it
    // stopped while its processes run on. So the names change only while orrery is stopped:
    // with --start, setup stops it for the change; without, it leaves the names as they are.
    let wanted = want_alias && !blocked;
    let waits = wanted != has_name && !options.check && !options.start && is_active(host, "orrery")?;
    let alias = if waits { has_name } else { wanted };
    let name_changes = alias != has_name;
    let declined = declined && !waits;
    let unit = unit_file(&account.name, &options.exe, &args, alias, declined);
    let gid = group_gid(host)?;
    let installed_rule = std::fs::read_to_string(sys(RULE_PATH));
    let strict = options.strict || (options.check && installed_rule.as_ref().is_ok_and(|r| r.starts_with(STRICT_MARK)));
    let units = if strict { Some(allowed_units(&account, &args, &config)?) } else { None };
    let rule = |gid: u32| match &units {
        Some(units) => strict_rule(&account.name, gid, units),
        None => normal_rule(&account.name, gid),
    };
    let rule_kind = if strict { "the strict rule" } else { "the normal rule" };

    if options.check {
        let mut report = Report { ok: true, ..Report::default() };
        report.line(format!("orrery setup for the user {}:", account.name));
        match gid {
            Some(gid) => report.found(Status::Ok, format!("The group {GROUP} is there (gid {gid}).")),
            None => report.found(Status::Update, format!("The group {GROUP} is missing.")),
        }
        check_file(&mut report, &sys(UNIT_PATH), &format!("/{UNIT_PATH}"), &unit);
        let rule = rule(gid.unwrap_or(u32::MAX));
        match &installed_rule {
            // The rules folder is readable only by root, so a check as the printer's user can't
            // tell which rule it is. What polkit allows, below, is what matters.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => report.found(
                Status::Unknown,
                format!("/{RULE_PATH} can't be read as {}. sudo orrery setup --check compares it.", account.name),
            ),
            _ => check_file(&mut report, &sys(RULE_PATH), &format!("/{RULE_PATH} ({rule_kind})"), &rule),
        }
        let running = is_active(host, "orrery")?;
        let state = format!("{SERVICE} is {}.", unit_state(host, "orrery")?);
        report.found(if running { Status::Ok } else { Status::Note }, state);
        if running {
            let socket = data_folder(&account, &args).join("comms/moonraker.sock");
            check_permissions(&mut report, host.ask_orrery(&socket, "orrery.machine.permissions"));
        } else {
            report.found(Status::Unknown, format!("What polkit allows {SERVICE}: setup asks it while it runs."));
        }
        match (moonraker_unit, want_alias) {
            (MoonrakerUnit::Alias, true) => report.found(Status::Ok, format!("{MOONRAKER} is a name for {SERVICE}.")),
            (MoonrakerUnit::Alias, false) => {
                report.found(Status::Update, format!("{MOONRAKER} is a name for {SERVICE}, but it shouldn't be."))
            }
            (MoonrakerUnit::Own, true) => report.found(
                Status::Update,
                format!("/{MOONRAKER_PATH} is Moonraker's own unit, so it isn't a name for {SERVICE}."),
            ),
            (MoonrakerUnit::Missing, true) => {
                report.found(Status::Update, format!("/{MOONRAKER_PATH}, the name for {SERVICE}, is missing."))
            }
            _ => report.found(Status::Note, format!("{MOONRAKER} is {}.", unit_state(host, "moonraker")?)),
        }
        if sys(SAVED_PATH).exists() {
            report.found(Status::Note, format!("Moonraker's own unit is saved as /{SAVED_PATH}."));
        }
        if !report.ok {
            report.line("To make the updates, run:");
            report.line(format!(
                "  sudo orrery setup{}{}",
                if strict { " --strict" } else { "" },
                if blocked { " --force" } else { "" }
            ));
            if blocked {
                keep_moonraker(&mut report);
            }
        }
        return Ok(report);
    }

    let mut report = Report { ok: true, ..Report::default() };
    report.line(format!("orrery setup for the user {}:", account.name));
    let gid = match gid {
        Some(gid) => {
            report.line(format!("  The group {GROUP} is already there (gid {gid})."));
            gid
        }
        None => {
            command(host, "groupadd", &["--system", GROUP])?;
            let gid = group_gid(host)?.with_context(|| format!("groupadd made {GROUP}, but getent can't find it"))?;
            report.line(format!("  Made the group {GROUP} (gid {gid})."));
            gid
        }
    };
    let unit_changed = install(&mut report, &sys(UNIT_PATH), &format!("/{UNIT_PATH}"), &unit)?;
    install(&mut report, &sys(RULE_PATH), &format!("/{RULE_PATH} ({rule_kind})"), &rule(gid))?;
    let stopped_for_name = name_changes && is_active(host, "orrery")?;
    if stopped_for_name {
        command(host, "systemctl", &["stop", SERVICE])?;
    }
    if alias {
        take_moonrakers_name(&mut report, host, &sys, moonraker_unit)?;
    } else if moonraker_unit == MoonrakerUnit::Alias {
        give_back_moonrakers_name(&mut report, &sys)?;
    }
    command(host, "systemctl", &["daemon-reload"])?;
    if alias {
        // Moonraker's unit is aside, and its name is orrery's: disabling it would disable orrery.
    } else if options.disable_moonraker {
        if unit_state(host, "moonraker")? == "not installed" {
            report.line(format!("  {MOONRAKER} isn't installed, so there's nothing to disable."));
        } else {
            command(host, "systemctl", &["disable", "--now", MOONRAKER])?;
            report.line(format!("  Stopped and disabled {MOONRAKER}."));
        }
    }
    let running = is_active(host, "orrery")?;
    if options.start && stopped_for_name {
        command(host, "systemctl", &["enable", SERVICE])?;
        command(host, "systemctl", &["start", SERVICE])?;
        report.line(format!("  Enabled {SERVICE}, and restarted it for its new names."));
    } else if options.start {
        command(host, "systemctl", &["enable", SERVICE])?;
        // restart, not start: an upgraded unit or binary must take effect.
        command(host, "systemctl", &[if running { "restart" } else { "start" }, SERVICE])?;
        report.line(format!("  Enabled {SERVICE} and {} it.", if running { "restarted" } else { "started" }));
    } else if running && unit_changed && !waits {
        // When setup leaves the names, its own message says to restart.
        report.line(format!("{SERVICE} runs with the old unit. To use the new one, run:"));
        report.line(format!("  sudo systemctl restart {SERVICE}"));
    } else if !running {
        report.line("orrery.service isn't running. To switch from Moonraker to orrery, run:");
        report.line("  sudo orrery setup --migrate");
    }
    if strict {
        report.line("The strict rule names each allowed service. After adding a service, run setup again:");
        report.line("  sudo orrery setup --strict");
    }
    if waits {
        let (what, force) = if wanted {
            (format!("give {SERVICE} the name {MOONRAKER}"), if options.force { " --force" } else { "" })
        } else {
            (format!("take the name {MOONRAKER} away from {SERVICE}"), "")
        };
        report.line(format!(
            "{SERVICE} runs, and has to restart for its names to change, so setup leaves them as they are."
        ));
        report.line(format!("To restart orrery and {what}, run:"));
        report.line(format!("  sudo orrery setup{flag} --start{force}"));
    } else if name_changes && alias {
        report.line(format!("{MOONRAKER} is now a name for {SERVICE}. To give the name back, run:"));
        report.line("  sudo orrery setup --no-alias-moonraker --start");
    }
    if blocked {
        report.line(format!(
            "/{MOONRAKER_PATH} is Moonraker's own unit, so orrery doesn't take its name, and services that start Moonraker stop orrery."
        ));
        report.line("To disable Moonraker and move its unit aside, so those services get orrery, run:");
        report.line(format!("  sudo orrery setup{flag} --force{}", if running { " --start" } else { "" }));
        keep_moonraker(&mut report);
    }
    Ok(report)
}

/// How to keep Moonraker's own unit, as when trying orrery beside Moonraker.
fn keep_moonraker(report: &mut Report) {
    report.line("To keep Moonraker's unit, and stop this note, run:");
    report.line("  sudo orrery setup --no-alias-moonraker");
}

/// What's at `/etc/systemd/system/moonraker.service`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoonrakerUnit {
    Missing,
    /// The symlink to `orrery.service`.
    Alias,
    /// Anything else: Moonraker's own unit, usually from its installer.
    Own,
}

fn moonraker_unit(path: &Path) -> Result<MoonrakerUnit> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MoonrakerUnit::Missing),
        Err(e) => Err(e).with_context(|| format!("can't read {}", path.display())),
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(path).with_context(|| format!("can't read {}", path.display()))?;
            Ok(if target.file_name() == Some(SERVICE.as_ref()) { MoonrakerUnit::Alias } else { MoonrakerUnit::Own })
        }
        Ok(_) => Ok(MoonrakerUnit::Own),
    }
}

/// Makes `moonraker.service` a name for orrery's unit: stops and disables Moonraker, moves its
/// own unit file aside (the caller checked `--force`), and makes the symlink.
fn take_moonrakers_name(
    report: &mut Report,
    host: &mut dyn Host,
    sys: &dyn Fn(&str) -> PathBuf,
    found: MoonrakerUnit,
) -> Result<()> {
    if found == MoonrakerUnit::Alias {
        report.line(format!("  {MOONRAKER} is already a name for {SERVICE}."));
        return Ok(());
    }
    // While its unit is still there to disable: a unit from a package, or Moonraker's own.
    if unit_state(host, "moonraker")? != "not installed" {
        command(host, "systemctl", &["disable", "--now", MOONRAKER])?;
        report.line(format!("  Stopped and disabled {MOONRAKER}."));
    }
    if found == MoonrakerUnit::Own {
        std::fs::rename(sys(MOONRAKER_PATH), sys(SAVED_PATH))
            .with_context(|| format!("can't move /{MOONRAKER_PATH} to /{SAVED_PATH}"))?;
        report.line(format!("  Moved Moonraker's own unit to /{SAVED_PATH}."));
    }
    std::os::unix::fs::symlink(SERVICE, sys(MOONRAKER_PATH))
        .with_context(|| format!("can't make /{MOONRAKER_PATH}"))?;
    report.line(format!("  Made {MOONRAKER} a name for {SERVICE}."));
    Ok(())
}

/// Takes `moonraker.service` away from orrery. Moonraker's own unit stays where `--force` put
/// it; `--remove` puts it back.
fn give_back_moonrakers_name(report: &mut Report, sys: &dyn Fn(&str) -> PathBuf) -> Result<()> {
    std::fs::remove_file(sys(MOONRAKER_PATH)).with_context(|| format!("can't remove /{MOONRAKER_PATH}"))?;
    report.line(format!("  Removed {MOONRAKER}, the name for {SERVICE}."));
    if sys(SAVED_PATH).exists() {
        report.line(format!("  Moonraker's own unit stays at /{SAVED_PATH}."));
    }
    Ok(())
}

/// Removes the unit, the rule, and the group, after stopping and disabling orrery.service.
fn remove(host: &mut dyn Host, sys: &dyn Fn(&str) -> PathBuf) -> Result<Report> {
    let mut report = Report { ok: true, ..Report::default() };
    let state = unit_state(host, "orrery")?;
    if state != "not installed" && state != "disabled and stopped" {
        command(host, "systemctl", &["disable", "--now", SERVICE])?;
        report.line(format!("Stopped and disabled {SERVICE}."));
    }
    // systemctl disable removes the alias of an enabled unit; this covers one that wasn't.
    if moonraker_unit(&sys(MOONRAKER_PATH))? == MoonrakerUnit::Alias {
        std::fs::remove_file(sys(MOONRAKER_PATH)).with_context(|| format!("can't remove /{MOONRAKER_PATH}"))?;
        report.line(format!("Removed {MOONRAKER}, the name for {SERVICE}."));
    }
    if sys(SAVED_PATH).exists() {
        if moonraker_unit(&sys(MOONRAKER_PATH))? == MoonrakerUnit::Missing {
            std::fs::rename(sys(SAVED_PATH), sys(MOONRAKER_PATH))
                .with_context(|| format!("can't move /{SAVED_PATH} back to /{MOONRAKER_PATH}"))?;
            report.line(format!("Put Moonraker's own unit back at /{MOONRAKER_PATH}."));
        } else {
            report.line(format!(
                "Moonraker's unit was written again at /{MOONRAKER_PATH}, so the older copy stays at /{SAVED_PATH}."
            ));
        }
    }
    for path in [UNIT_PATH, RULE_PATH] {
        match std::fs::remove_file(sys(path)) {
            Ok(()) => report.line(format!("Removed /{path}.")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => report.line(format!("/{path} wasn't there.")),
            Err(e) => return Err(e).with_context(|| format!("can't remove /{path}")),
        }
    }
    if group_gid(host)?.is_some() {
        command(host, "groupdel", &[GROUP])?;
        report.line(format!("Removed the group {GROUP}."));
    }
    command(host, "systemctl", &["daemon-reload"])?;
    let moonraker = unit_state(host, "moonraker")?;
    if moonraker.starts_with("disabled") {
        report.line("Moonraker is disabled. To turn it on again, run:");
        report.line(format!("  sudo systemctl enable --now {MOONRAKER}"));
    }
    Ok(report)
}

/// Who orrery runs as: the user who ran sudo, or for `--check` without sudo, the user running
/// it.
fn account(options: &Options, host: &mut dyn Host) -> Result<Account> {
    let key = match (&options.sudo_user, options.is_root) {
        (Some(name), _) if !name.is_empty() => name.clone(),
        (_, false) => options.uid.to_string(),
        _ => bail!(
            "orrery setup sets orrery up for the user who runs sudo, and SUDO_USER isn't set. Run it from the printer's user:\n  sudo orrery setup"
        ),
    };
    let (ok, out) = host.run("getent", &["passwd", &key])?;
    if !ok {
        bail!("getent can't find the user {key}");
    }
    let account = parse_passwd(&out).with_context(|| format!("can't read the user {key}'s entry: {out:?}"))?;
    if account.uid == 0 {
        bail!("orrery shouldn't run as root. Run setup with sudo from the printer's user:\n  sudo orrery setup");
    }
    Ok(account)
}

pub fn parse_passwd(entry: &str) -> Option<Account> {
    let fields: Vec<&str> = entry.lines().next()?.split(':').collect();
    let [name, _, uid, _, _, home, _] = fields.as_slice() else { return None };
    Some(Account { name: (*name).to_owned(), uid: uid.parse().ok()?, home: PathBuf::from(home) })
}

/// The group's id, or `None` when it isn't there.
fn group_gid(host: &mut dyn Host) -> Result<Option<u32>> {
    let (ok, out) = host.run("getent", &["group", GROUP])?;
    if !ok {
        return Ok(None);
    }
    parse_group_gid(&out).map(Some).with_context(|| format!("can't read the group {GROUP}'s entry: {out:?}"))
}

pub fn parse_group_gid(entry: &str) -> Option<u32> {
    entry.lines().next()?.split(':').nth(2)?.parse().ok()
}

/// Refuses polkit 0.105, which has no JavaScript rules and can't check a process's groups.
fn check_polkit(host: &mut dyn Host) -> Result<()> {
    let (ok, out) = host.run("pkaction", &["--version"]).context("can't run pkaction, so polkit may be missing")?;
    if !ok {
        bail!("pkaction --version failed, so orrery can't tell which polkit this is");
    }
    let version = parse_polkit_version(&out).with_context(|| format!("can't read polkit's version from {out:?}"))?;
    if version < (0, 106) {
        bail!(
            "This system has polkit {}.{}, which can't check which groups a process has. A rule for orrery would have to trust every program the printer's user runs. Moonraker's rule and its moonraker-admin group keep working, and orrery can use them: give orrery's unit SupplementaryGroups=moonraker-admin.",
            version.0,
            version.1
        );
    }
    Ok(())
}

/// polkit's version from `pkaction --version`, such as "pkaction version 122" or "0.105".
pub fn parse_polkit_version(text: &str) -> Option<(u32, u32)> {
    let word = text.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = word.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((major, minor))
}

/// The options `ExecStart` passes to orrery: the ones given, made absolute, or else the ones in
/// the installed unit, so an upgrade keeps them.
fn server_args(options: &Options, account: &Account, installed: Option<&str>) -> Result<Vec<String>> {
    let given = [("-d", &options.data_path), ("-c", &options.config_file), ("--orrery-config", &options.orrery_config)];
    if given.iter().all(|(_, v)| v.is_none()) {
        return Ok(installed.and_then(installed_args).unwrap_or_default());
    }
    let mut args = Vec::new();
    for (flag, value) in given {
        if let Some(path) = value {
            args.push(flag.to_owned());
            args.push(absolute(path, account, &options.cwd).display().to_string());
        }
    }
    Ok(args)
}

/// A path as orrery's service sees it: `~` is the account's home, and a relative path is
/// relative to where setup ran.
fn absolute(path: &Path, account: &Account, cwd: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => account.home.join(rest),
        Err(_) if path.is_relative() => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// The arguments after the program in an installed unit's `ExecStart`, as setup wrote them.
fn installed_args(unit: &str) -> Option<Vec<String>> {
    let line = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let mut words = split_quoted(line)?;
    if words.is_empty() {
        return None;
    }
    words.remove(0);
    Some(words)
}

/// Splits `"a" "b c"` into words, undoing [`quote`]'s escapes.
fn split_quoted(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek() == Some(&' ') {
            chars.next();
        }
        match chars.next() {
            None => return Some(words),
            Some('"') => {}
            Some(_) => return None,
        }
        let mut word = String::new();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => word.push(chars.next()?),
                '%' if chars.peek() == Some(&'%') => {
                    chars.next();
                    word.push('%');
                }
                c => word.push(c),
            }
        }
        words.push(word);
    }
}

/// A double-quoted `ExecStart` word: `%` doubled for systemd's specifiers, and `\` and `"`
/// escaped, so a path with spaces or quotes stays one word.
fn quote(word: &str) -> String {
    let mut out = String::from("\"");
    for c in word.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn unit_file(user: &str, exe: &Path, args: &[String], alias: bool, declined: bool) -> String {
    let mut exec = quote(&exe.display().to_string());
    for arg in args {
        exec.push(' ');
        exec.push_str(&quote(arg));
    }
    let (description, moonraker, install) = if alias {
        (
            "orrery, an API server for Klipper, also named moonraker.service",
            "# Units that start or wait for Moonraker get orrery: moonraker.service is a name for this unit.\n"
                .to_owned(),
            format!("{ALIAS_LINE}\n"),
        )
    } else {
        (
            "orrery, an API server for Klipper",
            format!(
                "{}# Both listen on port 7125, so starting one stops the other.\nConflicts={MOONRAKER}\n",
                if declined { format!("{NO_ALIAS_LINE}\n") } else { String::new() }
            ),
            String::new(),
        )
    };
    format!(
        "# Written by sudo orrery setup. Run it again instead of editing this file.\n\
         [Unit]\n\
         Description={description}\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         {moonraker}\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         SupplementaryGroups={GROUP}\n\
         ExecStart={exec}\n\
         Restart=always\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n\
         {install}"
    )
}

/// The part both rules share: the user, and the group check on the process.
fn rule_body(user: &str, gid: u32, condition: &str) -> String {
    let user = serde_json::to_string(user).expect("a string serializes");
    let actions: Vec<&str> = POWER_ACTIONS.iter().chain(PACKAGE_ACTIONS).copied().collect();
    let actions = serde_json::to_string(&actions).expect("strings serialize");
    // /proc/<pid>/status has "Groups:" then the process's group ids, separated by whitespace.
    let regex = serde_json::to_string(&format!(r"^Groups:.*\s{gid}(\s|$)")).expect("a string serializes");
    let mut out = String::new();
    let _ = write!(
        out,
        "polkit.addRule(function(action, subject) {{\n\
         \x20   var actions = {actions};\n\
         {condition}\
         \x20   if (!allowed || subject.user != {user}) {{\n\
         \x20       return polkit.Result.NOT_HANDLED;\n\
         \x20   }}\n\
         \x20   // Only processes that have the {GROUP} group, which orrery.service gets.\n\
         \x20   try {{\n\
         \x20       polkit.spawn([\"grep\", \"-Pq\", {regex}, \"/proc/\" + subject.pid + \"/status\"]);\n\
         \x20       return polkit.Result.YES;\n\
         \x20   }} catch (error) {{\n\
         \x20       return polkit.Result.NOT_HANDLED;\n\
         \x20   }}\n\
         }});\n"
    );
    out
}

/// Moonraker's rule for orrery's group: every unit, reboot, shutdown, and the PackageKit actions
/// the update manager needs.
pub fn normal_rule(user: &str, gid: u32) -> String {
    let condition = format!("    var allowed = actions.indexOf(action.id) >= 0 || action.id == \"{MANAGE_UNITS}\";\n");
    format!(
        "// orrery setup: normal rule. Written by sudo orrery setup; run it again instead of editing this file.\n\
         // orrery.service may manage every unit, reboot, shut down, and update packages.\n{}",
        rule_body(user, gid, &condition)
    )
}

/// The units the strict rule allows: names, and prefixes such as `klipper`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedUnits {
    pub names: Vec<String>,
    pub prefixes: Vec<String>,
}

/// Like the normal rule, but only for the units orrery allows.
pub fn strict_rule(user: &str, gid: u32, units: &AllowedUnits) -> String {
    let names = serde_json::to_string(&units.names).expect("strings serialize");
    let prefixes = serde_json::to_string(&units.prefixes).expect("strings serialize");
    let condition = format!(
        "    var names = {names};\n\
         \x20   var prefixes = {prefixes};\n\
         \x20   function allowedUnit(unit) {{\n\
         \x20       if (!unit) {{\n\
         \x20           return false;\n\
         \x20       }}\n\
         \x20       var name = unit.replace(/\\.service$/, \"\");\n\
         \x20       if (names.indexOf(name) >= 0) {{\n\
         \x20           return true;\n\
         \x20       }}\n\
         \x20       for (var i = 0; i < prefixes.length; i++) {{\n\
         \x20           if (name.indexOf(prefixes[i]) == 0) {{\n\
         \x20               return true;\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20       return false;\n\
         \x20   }}\n\
         \x20   var allowed = actions.indexOf(action.id) >= 0 ||\n\
         \x20       (action.id == \"{MANAGE_UNITS}\" && allowedUnit(action.lookup(\"unit\")));\n"
    );
    format!(
        "{STRICT_MARK}. Written by sudo orrery setup; run it again instead of editing this file.\n\
         // orrery.service may reboot, shut down, update packages, and manage only the units orrery allows:\n\
         // moonraker.asvc, units whose names start with {}, agents' units, and the units update manager entries restart.\n{}",
        units.prefixes.join(", "),
        rule_body(user, gid, &condition)
    )
}

/// The value of one of orrery's path options in `ExecStart`'s arguments.
fn arg_path(args: &[String], flag: &str) -> Option<PathBuf> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(PathBuf::from)
}

fn data_folder(account: &Account, args: &[String]) -> PathBuf {
    arg_path(args, "-d").unwrap_or_else(|| account.home.join("printer_data"))
}

/// The config the service will read, as orrery reads it: `moonraker.conf`, then `orrery.toml`.
fn load_config(account: &Account, args: &[String]) -> Result<crate::config::Config> {
    let data = data_folder(account, args);
    let config_file = arg_path(args, "-c").unwrap_or_else(|| data.join("config/moonraker.conf"));
    let (toml, required) = match arg_path(args, "--orrery-config") {
        Some(path) => (path, true),
        None => (config_file.with_file_name("orrery.toml"), false),
    };
    let mut config = crate::config::Config::load(&config_file).with_context(|| {
        format!("setup reads the service's config, as orrery does, and can't read {}", config_file.display())
    })?;
    if required || toml.exists() {
        config.load_toml(&toml)?;
    }
    Ok(config)
}

/// The units orrery allows, read as orrery reads them: `moonraker.asvc` in the data folder,
/// the names that are always allowed, and the system units of agents and of update manager
/// entries in the config.
fn allowed_units(account: &Account, args: &[String], config: &crate::config::Config) -> Result<AllowedUnits> {
    let asvc = data_folder(account, args).join("moonraker.asvc");
    // Read only: unlike orrery, setup runs as root, so it doesn't write the default list.
    let text = std::fs::read_to_string(&asvc).unwrap_or_else(|_| crate::services::DEFAULT_ALLOWED.to_owned());
    let mut names = crate::services::parse_allowed(&text);
    let (agents, _) = crate::agents::Agents::new(config, None)?;
    for (_, unit) in agents.units() {
        if unit.scope == crate::agents::Scope::System {
            let name = unit.name.strip_suffix(".service").unwrap_or(&unit.name).to_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    // The system units update manager entries restart. Their user units need no polkit rule.
    for name in crate::services::update_manager_units(config).0 {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    let prefixes = crate::services::ALWAYS_ALLOWED.iter().map(|p| (*p).to_owned()).collect();
    Ok(AllowedUnits { names, prefixes })
}

fn command(host: &mut dyn Host, program: &str, args: &[&str]) -> Result<()> {
    let (ok, _) = host.run(program, args)?;
    if !ok {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

fn is_active(host: &mut dyn Host, unit: &str) -> Result<bool> {
    let (_, out) = host.run("systemctl", &["is-active", &format!("{unit}.service")])?;
    Ok(out.trim() == "active")
}

/// A unit's state in words: "enabled and running", "disabled and stopped", "not installed".
fn unit_state(host: &mut dyn Host, unit: &str) -> Result<String> {
    let service = format!("{unit}.service");
    let (_, enabled) = host.run("systemctl", &["is-enabled", &service])?;
    let enabled = enabled.trim();
    if enabled.is_empty() || enabled == "not-found" {
        return Ok("not installed".to_owned());
    }
    let running = if is_active(host, unit)? { "running" } else { "stopped" };
    let enabled = if enabled == "enabled" { "enabled" } else { "disabled" };
    Ok(format!("{enabled} and {running}"))
}

fn check_file(report: &mut Report, path: &Path, name: &str, want: &str) {
    match std::fs::read_to_string(path) {
        Ok(have) if have == want => report.found(Status::Ok, format!("{name} is in place.")),
        Ok(_) => report.found(Status::Update, format!("{name} differs from what setup writes.")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            report.found(Status::Update, format!("{name} is missing."))
        }
        Err(e) => report.found(Status::Unknown, format!("{name} can't be read: {e}.")),
    }
}

/// What the running orrery says polkit allows it. Only orrery's own process has the group the
/// rule trusts, so setup can't ask polkit itself.
fn check_permissions(report: &mut Report, answer: Result<serde_json::Value>) {
    let answer = match answer {
        Ok(a) => a,
        Err(e) => {
            report.found(Status::Unknown, format!("setup can't ask {SERVICE} what polkit allows it: {e:#}."));
            return;
        }
    };
    if answer["checked"] != true {
        match answer["error"].as_str() {
            Some(e) => report.found(Status::Unknown, format!("{SERVICE} can't tell what polkit allows it: {e}.")),
            None => {
                report.found(Status::Unknown, format!("{SERVICE} hasn't asked polkit yet. Check again in a moment."))
            }
        }
        return;
    }
    let list = answer["permissions"].as_array().cloned().unwrap_or_default();
    let what = |allowed: bool| -> Vec<String> {
        list.iter().filter(|p| p["allowed"] == allowed).filter_map(|p| p["what"].as_str().map(str::to_owned)).collect()
    };
    let refused = what(false);
    if refused.is_empty() {
        report.found(Status::Ok, format!("polkit lets {SERVICE}: {}.", what(true).join("; ")));
    } else {
        report.found(Status::Update, format!("polkit doesn't let {SERVICE}: {}.", refused.join("; ")));
        report.found(
            Status::Note,
            format!("{SERVICE} asked polkit when it started: after sudo orrery setup, restart it and check again."),
        );
    }
}

/// Writes a file through a temporary file and a rename, unless it's already current. Returns
/// whether it changed.
fn install(report: &mut Report, path: &Path, name: &str, contents: &str) -> Result<bool> {
    let old = std::fs::read_to_string(path).ok();
    if old.as_deref() == Some(contents) {
        report.line(format!("  {name} is already current."));
        return Ok(false);
    }
    let dir = path.parent().context("a system file has a folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("can't make {}", dir.display()))?;
    let mut temp = tempfile_in(dir)?;
    use std::io::Write as _;
    temp.1.write_all(contents.as_bytes()).with_context(|| format!("can't write {}", temp.0.display()))?;
    drop(temp.1);
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&temp.0, path).with_context(|| format!("can't write {}", path.display()))?;
    report.line(format!("  {} {name}.", if old.is_some() { "Updated" } else { "Installed" }));
    Ok(true)
}

/// A new file in `dir` with a name nothing else uses.
fn tempfile_in(dir: &Path) -> Result<(PathBuf, std::fs::File)> {
    for n in 0..100u32 {
        let path = dir.join(format!(".orrery-setup-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("can't write in {}", dir.display())),
        }
    }
    bail!("can't make a temporary file in {}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    /// A system that records commands and answers from a script.
    #[derive(Default)]
    struct Fake {
        ran: Vec<String>,
        group: Option<u32>,
        enabled: HashMap<&'static str, &'static str>,
        active: HashSet<&'static str>,
        polkit: &'static str,
        /// What the running orrery answers, or `None` when it can't be reached.
        orrery: Option<serde_json::Value>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                polkit: "pkaction version 122\n",
                enabled: [("orrery.service", ""), ("moonraker.service", "enabled")].into_iter().collect(),
                active: ["moonraker.service"].into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl Host for Fake {
        fn ask_orrery(&mut self, socket: &Path, method: &str) -> Result<serde_json::Value> {
            assert!(socket.ends_with("printer_data/comms/moonraker.sock"), "{}", socket.display());
            assert_eq!(method, "orrery.machine.permissions");
            self.orrery.clone().context("connection refused")
        }

        fn run(&mut self, program: &str, args: &[&str]) -> Result<(bool, String)> {
            let line = format!("{program} {}", args.join(" "));
            let reply = match (program, args) {
                ("getent", ["passwd", "printer" | "1000"]) => {
                    (true, "printer:x:1000:1000::/home/printer:/bin/bash\n".into())
                }
                ("getent", ["group", _]) => match self.group {
                    Some(gid) => (true, format!("{GROUP}:x:{gid}:\n")),
                    None => (false, String::new()),
                },
                ("groupadd", _) => {
                    self.group = Some(994);
                    (true, String::new())
                }
                ("groupdel", _) => {
                    self.group = None;
                    (true, String::new())
                }
                ("pkaction", _) => (true, self.polkit.to_owned()),
                ("systemctl", ["is-enabled", unit]) => {
                    (false, self.enabled.get(unit).map_or("not-found", |s| s).to_owned() + "\n")
                }
                ("systemctl", ["is-active", unit]) => {
                    let active = self.active.contains(unit);
                    (active, if active { "active\n" } else { "inactive\n" }.to_owned())
                }
                ("systemctl", ["disable", "--now", unit]) => {
                    let unit: &'static str = if *unit == SERVICE { SERVICE } else { MOONRAKER };
                    self.enabled.insert(unit, "disabled");
                    self.active.remove(unit);
                    (true, String::new())
                }
                ("systemctl", ["enable", _]) => {
                    self.enabled.insert(SERVICE, "enabled");
                    (true, String::new())
                }
                ("systemctl", ["start" | "restart", _]) => {
                    self.active.insert(SERVICE);
                    (true, String::new())
                }
                ("systemctl", ["stop", _]) => {
                    self.active.remove(SERVICE);
                    (true, String::new())
                }
                ("systemctl", _) => (true, String::new()),
                _ => (false, String::new()),
            };
            if !matches!(args.first(), Some(&"is-enabled" | &"is-active" | &"group" | &"passwd" | &"--version")) {
                self.ran.push(line);
            }
            Ok(reply)
        }
    }

    /// orrery.toml for a printer that keeps Moonraker's unit.
    const NO_ALIAS: &str = "[moonraker_compat]\nservice_alias = false\n";

    /// Options whose service reads a moonraker.conf in `root`, and `toml` as orrery.toml.
    fn options_in(root: &Path, toml: &str) -> Options {
        let config = root.join("printer_data/config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("moonraker.conf"), "[server]\n").unwrap();
        if !toml.is_empty() {
            std::fs::write(config.join("orrery.toml"), toml).unwrap();
        }
        Options { data_path: Some(root.join("printer_data")), ..options() }
    }

    fn options() -> Options {
        Options {
            sudo_user: Some("printer".into()),
            is_root: true,
            uid: 0,
            exe: "/home/printer/orrery-test/bin/orrery".into(),
            cwd: "/home/printer".into(),
            ..Options::default()
        }
    }

    #[test]
    fn units_quote_their_words_and_keep_options_on_upgrade() {
        let args = vec!["--orrery-config".to_owned(), "/home/printer/my 100% \"x\".toml".to_owned()];
        let unit = unit_file("printer", Path::new("/opt/orrery/orrery"), &args, false, false);
        assert!(unit.contains("User=printer\nSupplementaryGroups=orrery-admin\n"));
        assert!(
            unit.contains(r#"ExecStart="/opt/orrery/orrery" "--orrery-config" "/home/printer/my 100%% \"x\".toml""#)
        );
        assert!(unit.contains("Conflicts=moonraker.service\n"));
        assert_eq!(installed_args(&unit).unwrap(), args);
        let account = Account { name: "printer".into(), uid: 1000, home: "/home/printer".into() };
        // Without new options, an upgrade keeps the installed ones.
        assert_eq!(server_args(&options(), &account, Some(&unit)).unwrap(), args);
        let given = Options { orrery_config: Some("~/o.toml".into()), data_path: Some("pd".into()), ..options() };
        assert_eq!(
            server_args(&given, &account, Some(&unit)).unwrap(),
            ["-d", "/home/printer/pd", "--orrery-config", "/home/printer/o.toml"]
        );
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_polkit_version("pkaction version 122\n"), Some((122, 0)));
        assert_eq!(parse_polkit_version("pkaction version 0.105\n"), Some((0, 105)));
        assert_eq!(parse_polkit_version("nothing"), None);
        assert_eq!(parse_group_gid("orrery-admin:x:994:\n"), Some(994));
        let a = parse_passwd("printer:x:1000:1000:,,,:/home/printer:/bin/bash\n").unwrap();
        assert_eq!((a.name.as_str(), a.uid, a.home.as_path()), ("printer", 1000, Path::new("/home/printer")));
    }

    #[test]
    fn rules_check_the_user_and_the_group() {
        let rule = normal_rule("printer", 994);
        assert!(rule.starts_with("// orrery setup: normal rule"));
        assert!(rule.contains(r#"subject.user != "printer""#));
        assert!(rule.contains(r#""^Groups:.*\\s994(\\s|$)""#));
        assert!(rule.contains("org.freedesktop.login1.reboot-multiple-sessions"));
        assert!(rule.contains("org.freedesktop.packagekit.system-update"));
        assert!(!rule.contains("packagekit.package-remove"));
        let units = AllowedUnits { names: vec!["crowsnest".into(), "it\"s".into()], prefixes: vec!["klipper".into()] };
        let rule = strict_rule("printer", 994, &units);
        assert!(rule.starts_with(STRICT_MARK));
        assert!(rule.contains(r#"var names = ["crowsnest","it\"s"];"#));
        assert!(rule.contains(r#"allowedUnit(action.lookup("unit"))"#));
        assert!(rule.contains("org.freedesktop.packagekit.package-install"));
    }

    #[test]
    fn setup_installs_checks_and_removes() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), NO_ALIAS);
        let mut host = Fake::new();
        // --check before anything is there.
        let check = Options { check: true, ..base.clone() };
        let report = run(&check, root.path(), &mut host).unwrap();
        assert!(!report.ok, "{:#?}", report.lines);
        assert!(host.ran.is_empty(), "--check changes nothing: {:?}", host.ran);

        let report = run(&base, root.path(), &mut host).unwrap();
        assert!(
            report.lines.iter().any(|l| l.contains("Made the group orrery-admin (gid 994)")),
            "{:#?}",
            report.lines
        );
        assert_eq!(host.ran, ["groupadd --system orrery-admin", "systemctl daemon-reload"]);
        let unit = std::fs::read_to_string(root.path().join(UNIT_PATH)).unwrap();
        assert!(unit.contains(r#"ExecStart="/home/printer/orrery-test/bin/orrery""#));
        assert!(unit.contains("Conflicts=moonraker.service\n") && !unit.contains("Alias="), "{unit}");
        let rule = std::fs::read_to_string(root.path().join(RULE_PATH)).unwrap();
        assert!(rule.contains(r"\\s994(\\s|$)"));
        assert!(run(&check, root.path(), &mut host).unwrap().ok);

        // Again: nothing to change.
        let report = run(&base, root.path(), &mut host).unwrap();
        assert_eq!(report.lines.iter().filter(|l| l.contains("already current")).count(), 2, "{:#?}", report.lines);

        // --check finds a file someone changed.
        std::fs::write(root.path().join(RULE_PATH), "edited").unwrap();
        let report = run(&check, root.path(), &mut host).unwrap();
        assert!(!report.ok && report.lines.iter().any(|l| l.contains("differs")), "{:#?}", report.lines);

        // --migrate: Moonraker stops and orrery starts.
        host.ran.clear();
        let migrate = Options { disable_moonraker: true, start: true, ..base.clone() };
        run(&migrate, root.path(), &mut host).unwrap();
        assert_eq!(
            host.ran,
            [
                "systemctl daemon-reload",
                "systemctl disable --now moonraker.service",
                "systemctl enable orrery.service",
                "systemctl start orrery.service"
            ]
        );
        // An upgrade with --start restarts it.
        host.ran.clear();
        run(&Options { start: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(host.ran.last().unwrap(), "systemctl restart orrery.service");

        host.ran.clear();
        let report = run(&Options { remove: true, ..base }, root.path(), &mut host).unwrap();
        assert_eq!(
            host.ran,
            ["systemctl disable --now orrery.service", "groupdel orrery-admin", "systemctl daemon-reload"]
        );
        assert!(!root.path().join(UNIT_PATH).exists() && !root.path().join(RULE_PATH).exists());
        assert!(report.lines.iter().any(|l| l.contains("enable --now moonraker.service")), "{:#?}", report.lines);
    }

    #[test]
    fn moonrakers_name_follows_the_config_and_needs_force_for_its_unit() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), "");
        std::fs::create_dir_all(root.path().join("etc/systemd/system")).unwrap();
        let moonraker = root.path().join(MOONRAKER_PATH);
        std::fs::write(&moonraker, "[Unit]\nDescription=Moonraker's own\n").unwrap();
        let mut host = Fake::new();

        // On by default, but Moonraker's own unit is in the way: setup works as without the
        // name, and says how to take it or keep Moonraker's.
        let report = run(&base, root.path(), &mut host).unwrap();
        assert_eq!(host.ran, ["groupadd --system orrery-admin", "systemctl daemon-reload"]);
        assert!(std::fs::read_to_string(&moonraker).unwrap().contains("Moonraker's own"));
        let unit = std::fs::read_to_string(root.path().join(UNIT_PATH)).unwrap();
        assert!(unit.contains("Conflicts=moonraker.service\n") && !unit.contains("Alias="), "{unit}");
        assert!(report.lines.contains(&"  sudo orrery setup --force".to_owned()), "{:#?}", report.lines);
        assert_eq!(report.lines.last().unwrap(), "  sudo orrery setup --no-alias-moonraker", "{:#?}", report.lines);
        let check = Options { check: true, ..base.clone() };
        let report = run(&check, root.path(), &mut host).unwrap();
        assert!(!report.ok, "{:#?}", report.lines);
        assert!(report.lines.contains(&"  sudo orrery setup --force".to_owned()), "{:#?}", report.lines);

        // --force: Moonraker stops, its unit goes aside, and the name is orrery's.
        host.ran.clear();
        let report = run(&Options { force: true, start: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(
            host.ran,
            [
                "systemctl disable --now moonraker.service",
                "systemctl daemon-reload",
                "systemctl enable orrery.service",
                "systemctl start orrery.service"
            ],
            "{:#?}",
            report.lines
        );
        assert_eq!(std::fs::read_link(&moonraker).unwrap(), Path::new(SERVICE));
        assert!(std::fs::read_to_string(root.path().join(SAVED_PATH)).unwrap().contains("Moonraker's own"));
        let unit = std::fs::read_to_string(root.path().join(UNIT_PATH)).unwrap();
        assert!(unit.ends_with("WantedBy=multi-user.target\nAlias=moonraker.service\n"), "{unit}");
        assert!(!unit.contains("Conflicts="), "{unit}");

        // An upgrade: the name stays, and only the restart touches orrery.
        host.ran.clear();
        let report = run(&Options { start: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(
            host.ran,
            ["systemctl daemon-reload", "systemctl enable orrery.service", "systemctl restart orrery.service"],
            "{:#?}",
            report.lines
        );
        let report = run(&check, root.path(), &mut host).unwrap();
        assert!(report.ok, "{:#?}", report.lines);
        assert!(report.lines.contains(&"  ok       moonraker.service is a name for orrery.service.".to_owned()));
        assert!(report.lines.iter().any(|l| l.contains("saved as /etc/systemd/system/moonraker.service.orrery-saved")));

        // A Moonraker install writes its unit again: --check says so, and --force takes it back.
        std::fs::remove_file(&moonraker).unwrap();
        std::fs::write(&moonraker, "[Unit]\nDescription=Moonraker, again\n").unwrap();
        let report = run(&check, root.path(), &mut host).unwrap();
        assert!(!report.ok, "{:#?}", report.lines);
        host.ran.clear();
        run(&Options { force: true, start: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(std::fs::read_link(&moonraker).unwrap(), Path::new(SERVICE));
        assert!(std::fs::read_to_string(root.path().join(SAVED_PATH)).unwrap().contains("again"));

        // service_alias = false: while orrery runs, the name stays until setup may restart it.
        std::fs::write(root.path().join("printer_data/config/orrery.toml"), NO_ALIAS).unwrap();
        host.ran.clear();
        let report = run(&base, root.path(), &mut host).unwrap();
        assert_eq!(host.ran, ["systemctl daemon-reload"], "{:#?}", report.lines);
        assert_eq!(moonraker_unit(&moonraker).unwrap(), MoonrakerUnit::Alias);
        assert_eq!(report.lines.last().unwrap(), "  sudo orrery setup --start", "{:#?}", report.lines);
        // With --start, it goes, and Moonraker's unit stays aside.
        host.ran.clear();
        let report = run(&Options { start: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(
            host.ran,
            [
                "systemctl stop orrery.service",
                "systemctl daemon-reload",
                "systemctl enable orrery.service",
                "systemctl start orrery.service"
            ],
            "{:#?}",
            report.lines
        );
        assert_eq!(moonraker_unit(&moonraker).unwrap(), MoonrakerUnit::Missing);
        assert!(std::fs::read_to_string(root.path().join(SAVED_PATH)).unwrap().contains("again"));
        assert!(report.lines.iter().any(|l| l.contains("stays at /etc/systemd/system/moonraker.service.orrery-saved")));
        let unit = std::fs::read_to_string(root.path().join(UNIT_PATH)).unwrap();
        assert!(unit.contains("Conflicts=moonraker.service\n") && !unit.contains("Alias="), "{unit}");
        assert!(run(&check, root.path(), &mut host).unwrap().ok);
    }

    #[test]
    fn the_flags_choose_and_later_runs_keep_the_choice() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), "");
        let mut host = Fake::new();
        host.enabled.insert(MOONRAKER, "not-found");
        host.active.clear();
        let unit = || std::fs::read_to_string(root.path().join(UNIT_PATH)).unwrap();

        // --no-alias-moonraker: no name, and the unit records the choice.
        let report = run(&Options { alias: Some(false), ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Missing);
        assert!(unit().contains(NO_ALIAS_LINE) && unit().contains("Conflicts="), "{}", unit());
        assert!(!report.lines.iter().any(|l| l.contains("--no-alias")), "{:#?}", report.lines);
        // A run without a flag, as after an upgrade, keeps it, and --check agrees.
        run(&base, root.path(), &mut host).unwrap();
        assert!(unit().contains(NO_ALIAS_LINE));
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Missing);
        assert!(run(&Options { check: true, ..base.clone() }, root.path(), &mut host).unwrap().ok);

        // --alias-moonraker: the name, with the hint to give it back.
        let report = run(&Options { alias: Some(true), ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Alias);
        assert!(unit().contains(ALIAS_LINE) && !unit().contains(NO_ALIAS_LINE), "{}", unit());
        assert_eq!(
            report.lines.last().unwrap(),
            "  sudo orrery setup --no-alias-moonraker --start",
            "{:#?}",
            report.lines
        );
        run(&base, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Alias);

        // orrery.toml wins over the recorded choice, and records its own.
        std::fs::write(root.path().join("printer_data/config/orrery.toml"), NO_ALIAS).unwrap();
        run(&base, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Missing);
        assert!(!unit().contains(NO_ALIAS_LINE), "{}", unit());
        // A flag wins over orrery.toml, for that run.
        run(&Options { alias: Some(true), ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Alias);

        // While orrery runs, a flag's change waits for --start, and the message repeats the flag.
        std::fs::remove_file(root.path().join("printer_data/config/orrery.toml")).unwrap();
        host.active = [SERVICE].into_iter().collect();
        let report = run(&Options { alias: Some(false), ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Alias);
        assert_eq!(
            report.lines.last().unwrap(),
            "  sudo orrery setup --no-alias-moonraker --start",
            "{:#?}",
            report.lines
        );
        run(&Options { alias: Some(false), start: true, ..base }, root.path(), &mut host).unwrap();
        assert_eq!(moonraker_unit(&root.path().join(MOONRAKER_PATH)).unwrap(), MoonrakerUnit::Missing);
        assert!(unit().contains(NO_ALIAS_LINE));
    }

    #[test]
    fn remove_puts_moonrakers_unit_back() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), "");
        std::fs::create_dir_all(root.path().join("etc/systemd/system")).unwrap();
        let moonraker = root.path().join(MOONRAKER_PATH);
        std::fs::write(&moonraker, "[Unit]\nDescription=Moonraker's own\n").unwrap();
        let mut host = Fake::new();
        run(&Options { force: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(std::fs::read_link(&moonraker).unwrap(), Path::new(SERVICE));
        let report = run(&Options { remove: true, ..base }, root.path(), &mut host).unwrap();
        assert!(std::fs::read_to_string(&moonraker).unwrap().contains("Moonraker's own"), "{:#?}", report.lines);
        assert!(!root.path().join(SAVED_PATH).exists());
        assert!(report.lines.iter().any(|l| l.contains("enable --now moonraker.service")), "{:#?}", report.lines);
    }

    #[test]
    fn a_running_orrery_gets_moonrakers_name_only_with_start() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), "");
        std::fs::create_dir_all(root.path().join("etc/systemd/system")).unwrap();
        let moonraker = root.path().join(MOONRAKER_PATH);
        std::fs::write(&moonraker, "[Unit]\nDescription=Moonraker's own\n").unwrap();
        let mut host = Fake::new();
        host.active = [SERVICE].into_iter().collect();
        // Without --start, setup restarts nothing, and moves nothing: the names stay.
        let report = run(&Options { force: true, ..base.clone() }, root.path(), &mut host).unwrap();
        assert_eq!(host.ran, ["groupadd --system orrery-admin", "systemctl daemon-reload"], "{:#?}", report.lines);
        assert!(std::fs::read_to_string(&moonraker).unwrap().contains("Moonraker's own"));
        let unit = std::fs::read_to_string(root.path().join(UNIT_PATH)).unwrap();
        assert!(unit.contains("Conflicts=moonraker.service\n") && !unit.contains("Alias="), "{unit}");
        assert_eq!(report.lines.last().unwrap(), "  sudo orrery setup --start --force", "{:#?}", report.lines);
        assert!(!report.lines.iter().any(|l| l.contains("old unit")), "{:#?}", report.lines);
        // Without --force too, it says both.
        let report = run(&base, root.path(), &mut host).unwrap();
        assert!(report.lines.contains(&"  sudo orrery setup --force --start".to_owned()), "{:#?}", report.lines);
        // With --start, it's one stop and one start, with no Moonraker unit to move.
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), "");
        host.enabled.insert(MOONRAKER, "not-found");
        host.ran.clear();
        host.group = None;
        let report = run(&Options { start: true, ..base }, root.path(), &mut host).unwrap();
        assert_eq!(
            host.ran,
            [
                "groupadd --system orrery-admin",
                "systemctl stop orrery.service",
                "systemctl daemon-reload",
                "systemctl enable orrery.service",
                "systemctl start orrery.service"
            ]
        );
        assert!(report.lines.iter().any(|l| l.contains("restarted it for its new names")), "{:#?}", report.lines);
    }

    #[test]
    fn setup_refuses_without_root_and_on_old_polkit() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), NO_ALIAS);
        let mut host = Fake::new();
        let e = run(&Options { is_root: false, ..base.clone() }, root.path(), &mut host).unwrap_err();
        assert!(format!("{e:#}").contains("sudo orrery setup"));
        host.polkit = "pkaction version 0.105\n";
        let e = run(&base, root.path(), &mut host).unwrap_err();
        assert!(format!("{e:#}").contains("polkit 0.105"), "{e:#}");
        assert!(host.ran.is_empty(), "nothing changes: {:?}", host.ran);
        // --check works without sudo, for the user running it.
        let check = Options { check: true, is_root: false, sudo_user: None, uid: 1000, ..base };
        host.polkit = "pkaction version 122\n";
        assert!(run(&check, root.path(), &mut host).unwrap().lines[0].contains("printer"));
        // A config that can't be read stops setup before it changes anything.
        let e = run(&options(), root.path(), &mut host).unwrap_err();
        assert!(format!("{e:#}").contains("setup reads the service's config"), "{e:#}");
        assert!(host.ran.is_empty(), "nothing changes: {:?}", host.ran);
    }

    #[test]
    fn wraps_long_findings() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("short", 40), ["short"]);
        assert_eq!(wrap("a /a/very/long/path b", 5), ["a", "/a/very/long/path", "b"]);
        let mut report = Report::default();
        report.found(Status::Ok, "word ".repeat(40).trim());
        assert!(report.lines.len() > 1 && report.lines.iter().all(|l| l.chars().count() <= CHECK_WIDTH));
        assert!(report.lines[0].starts_with("  ok       word") && report.lines[1].starts_with("           word"));
    }

    #[test]
    fn check_asks_the_running_orrery_what_polkit_allows() {
        let root = tempfile::tempdir().unwrap();
        let base = options_in(root.path(), NO_ALIAS);
        let mut host = Fake::new();
        run(&base, root.path(), &mut host).unwrap();
        let check = Options { check: true, ..base.clone() };
        let lines = |host: &mut Fake| run(&check, root.path(), host).unwrap();
        let report = lines(&mut host);
        assert!(
            report.lines.contains(&"  note     orrery.service is not installed.".to_owned()),
            "{:#?}",
            report.lines
        );
        assert!(
            report
                .lines
                .contains(&"  unknown  What polkit allows orrery.service: setup asks it while it runs.".to_owned())
        );

        host.active.insert("orrery.service");
        let allowed = |a: bool| {
            json!({"checked": true, "error": null, "permissions": [
            {"what": "start, stop, or restart services", "allowed": true},
            {"what": "reboot the machine", "allowed": a}]})
        };
        host.orrery = Some(allowed(true));
        let report = lines(&mut host);
        assert!(report.ok, "{:#?}", report.lines);
        assert!(report.lines.contains(
            &"  ok       polkit lets orrery.service: start, stop, or restart services; reboot the machine.".to_owned()
        ));
        host.orrery = Some(allowed(false));
        let report = lines(&mut host);
        assert!(!report.ok);
        assert!(report.lines.contains(&"  update   polkit doesn't let orrery.service: reboot the machine.".to_owned()));
        let text = report.lines.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(text.contains("restart it and check again"), "{:#?}", report.lines);
        host.orrery = Some(json!({"checked": false, "error": null}));
        assert!(lines(&mut host).lines.iter().any(|l| l.contains("hasn't asked polkit yet")));
        host.orrery = None;
        let report = lines(&mut host);
        assert!(report.lines.iter().any(|l| l.contains("can't ask orrery.service what polkit allows it")));
        assert!(report.ok, "not reaching orrery isn't a missing part: {:#?}", report.lines);

        // The rules folder is root's: a rule that can't be read isn't called missing.
        if !rustix::process::geteuid().is_root() {
            use std::os::unix::fs::PermissionsExt;
            let rule = root.path().join(RULE_PATH);
            std::fs::set_permissions(&rule, std::fs::Permissions::from_mode(0o000)).unwrap();
            host.orrery = Some(allowed(true));
            let report = lines(&mut host);
            std::fs::set_permissions(&rule, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(report.ok, "{:#?}", report.lines);
            assert!(
                report
                    .lines
                    .iter()
                    .any(|l| l.starts_with("  unknown  /etc/polkit-1/rules.d/orrery.rules can't be read as printer.")),
                "{:#?}",
                report.lines
            );
            assert!(!report.lines.iter().any(|l| l.contains("missing")), "{:#?}", report.lines);
        }
    }

    #[test]
    fn strict_rule_lists_the_allowed_units() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("printer_data");
        std::fs::create_dir_all(data.join("config")).unwrap();
        std::fs::write(data.join("config/moonraker.conf"), "[server]\n").unwrap();
        std::fs::write(data.join("config/orrery.toml"), "[agent.cam]\nsystemd_unit = \"orrery-cam\"\n").unwrap();
        std::fs::write(data.join("moonraker.asvc"), "crowsnest\nsonar.service\n").unwrap();
        let account = Account { name: "printer".into(), uid: 1000, home: home.path().to_path_buf() };
        let config = load_config(&account, &[]).unwrap();
        let units = allowed_units(&account, &[], &config).unwrap();
        assert_eq!(units.names, ["crowsnest", "orrery-cam", "sonar"]);
        assert_eq!(units.prefixes, ["moonraker", "klipper", "orrery"]);
    }
}
