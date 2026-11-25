//! The commands an entry runs: its preflight, a check that the new version can run before it
//! takes effect (docs/design/2026-11-22-self-update.md, "Preflight for every entry"), and its
//! `install` and `upgrade` (docs/design/2026-11-24-update-installs.md).
//!
//! A command is a shell line, run with `sh -c`, or a list of words, run without a shell. It
//! runs as orrery's user, with stdin closed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::packagekit::Messages;

/// How long a preflight check may run.
pub const PREFLIGHT_LIMIT: Duration = Duration::from_secs(60);
/// How long an `install` or `upgrade` command may run. No command should come close, but a
/// hung one ends.
pub const COMMAND_LIMIT: Duration = Duration::from_secs(30 * 60);
/// How many lines of a failed command's output its message keeps.
const TAIL: usize = 10;

/// A command, as the entry writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `"./install.sh --unattended"`, run with `sh -c`.
    Shell(String),
    /// `["./install.sh", "--unattended"]`, run without a shell.
    Words(Vec<String>),
}

impl Command {
    /// The command as messages show it.
    pub fn shown(&self) -> String {
        match self {
            Self::Shell(line) => line.clone(),
            Self::Words(words) => words.join(" "),
        }
    }

    /// The first word of a list of words.
    pub fn first_word(&self) -> Option<&str> {
        match self {
            Self::Shell(_) => None,
            Self::Words(words) => words.first().map(String::as_str),
        }
    }

    /// What to run, in `dir`: a word with a `/` is a path from `dir`, and another first word
    /// is a program on `PATH`.
    pub fn spec(&self, dir: &Path) -> Spec {
        match self {
            Self::Shell(line) => Spec::shell(line),
            Self::Words(words) => {
                let first = &words[0];
                let program = if first.contains('/') { dir.join(first) } else { PathBuf::from(first) };
                Spec { shown: self.shown(), program, args: words[1..].to_vec() }
            }
        }
    }
}

/// A list of words, for tests.
#[cfg(test)]
pub fn words(w: &[&str]) -> Option<Command> {
    Some(Command::Words(w.iter().map(|s| (*s).to_owned()).collect()))
}

/// An entry's `install`: what the updater does when it installs the entry from nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// `install = true`: the release works as placed.
    Bare,
    /// The release is placed, then the command runs.
    Run(Command),
}

impl Install {
    pub fn command(&self) -> Option<&Command> {
        match self {
            Self::Bare => None,
            Self::Run(c) => Some(c),
        }
    }
}

/// A command from an option: a shell line or a non-empty list of words. `None` when the option
/// is missing.
pub fn parse(section: &str, key: &str, value: Option<&Value>) -> Result<Option<Command>, String> {
    let bad = |v: &Value| {
        format!(
            r#"[{section}] {key}: expected a shell line, such as "./{key}.sh --quiet", or a list of words, such as ["./{key}.sh", "--quiet"], got {v}"#
        )
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(line)) if !line.trim().is_empty() => Ok(Some(Command::Shell(line.clone()))),
        Some(v @ Value::Array(items)) => {
            let words: Option<Vec<String>> = items.iter().map(|i| i.as_str().map(str::to_owned)).collect();
            match words {
                Some(w) if !w.is_empty() && !w[0].is_empty() => Ok(Some(Command::Words(w))),
                _ => Err(bad(v)),
            }
        }
        Some(v) => Err(bad(v)),
    }
}

/// An `install` option: `true`, `false`, or a command.
pub fn parse_install(section: &str, value: Option<&Value>) -> Result<Option<Install>, String> {
    match value {
        Some(Value::Bool(true)) => Ok(Some(Install::Bare)),
        Some(Value::Bool(false)) => Ok(None),
        other => Ok(parse(section, "install", other)?.map(Install::Run)),
    }
}

/// A program to run, and how messages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub shown: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Spec {
    pub fn shell(line: &str) -> Self {
        Self { shown: line.to_owned(), program: "sh".into(), args: vec!["-c".into(), line.to_owned()] }
    }
}

/// How a command runs.
pub struct Run<'a> {
    /// What the command is, for messages: "preflight check", "install command".
    pub what: &'a str,
    /// The folder it runs in.
    pub dir: &'a Path,
    /// A folder to put first on `PATH`, such as the one with a release's new programs.
    pub bin: Option<&'a Path>,
    pub env: &'a [(String, String)],
    pub limit: Duration,
    /// Where each line of its output goes, as it comes.
    pub out: Option<&'a Messages>,
}

/// Runs `spec`. Exit status 0 passes. A failure's message names the command and ends with the
/// last lines of its output.
pub async fn run(spec: &Spec, r: &Run<'_>) -> Result<(), String> {
    let (what, shown) = (r.what, &spec.shown);
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(r.dir)
        .envs(r.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(bin) = r.bin {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        if let Ok(joined) = std::env::join_paths(paths) {
            cmd.env("PATH", joined);
        }
    }
    // A program written a moment ago can be "busy" while a process another thread started
    // still holds it open for writing; that ends when the process runs its own program.
    let mut tries = 0;
    let mut child = loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::TXTBSY.raw_os_error()) && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => break result.map_err(|e| format!("The {what} `{shown}` can't start: {e}"))?,
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut kept: Vec<String> = Vec::new();
    let work = async {
        while let Some(line) = rx.recv().await {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(out) = r.out {
                let _ = out.send(line.clone());
            }
            kept.push(line);
            if kept.len() > TAIL {
                kept.remove(0);
            }
        }
        child.wait().await
    };
    let status = match tokio::time::timeout(r.limit, work).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("The {what} `{shown}` failed: {e}")),
        Err(_) => {
            let secs = r.limit.as_secs();
            let limit = if secs >= 60 && secs.is_multiple_of(60) {
                let m = secs / 60;
                format!("{m} {}", if m == 1 { "minute" } else { "minutes" })
            } else {
                format!("{secs} {}", if secs == 1 { "second" } else { "seconds" })
            };
            return Err(format!("The {what} `{shown}` didn't finish in {limit}."));
        }
    };
    if status.success() {
        return Ok(());
    }
    let tail = kept.join("\n");
    let status = status.code().map_or_else(|| "a signal".to_owned(), |c| format!("exit status {c}"));
    Err(if tail.is_empty() {
        format!("The {what} `{shown}` failed with {status}.")
    } else {
        format!("The {what} `{shown}` failed with {status}:\n{tail}")
    })
}

/// Runs a preflight check: `program` with `args`, in `dir`. `name` is what the user knows the
/// program as, since an update runs it from a hidden file.
pub async fn preflight(name: &str, program: &Path, args: &[String], dir: &Path) -> Result<(), String> {
    let shown = std::iter::once(name.to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
    let spec = Spec { shown, program: program.to_path_buf(), args: args.to_vec() };
    run(&spec, &Run { what: "preflight check", dir, bin: None, env: &[], limit: PREFLIGHT_LIMIT, out: None }).await
}

/// Runs a preflight check written as a shell line, in `dir`, with `bin`, the folder of the new
/// version's programs, first on `PATH`.
pub async fn preflight_shell(line: &str, dir: &Path, bin: Option<&Path>) -> Result<(), String> {
    run(&Spec::shell(line), &Run { what: "preflight check", dir, bin, env: &[], limit: PREFLIGHT_LIMIT, out: None })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn quick<'a>(dir: &'a Path, limit: Duration) -> Run<'a> {
        Run { what: "preflight check", dir, bin: None, env: &[], limit, out: None }
    }

    #[tokio::test]
    async fn passes_fails_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", "test \"$1\" = --check && test -f marker");
        std::fs::write(dir.path().join("marker"), "").unwrap();
        preflight("ok", &ok, &["--check".into()], dir.path()).await.unwrap();

        // An update runs the new program from a hidden file; the message names the program.
        let bad =
            script(dir.path(), ".tool.download", "for i in $(seq 1 15); do echo line $i; done; echo oops >&2; exit 3");
        let err = preflight("tool", &bad, &["--check".into()], dir.path()).await.unwrap_err();
        assert!(err.starts_with("The preflight check `tool --check` failed with exit status 3:\n"), "{err}");
        assert!(err.contains("line 15") && err.ends_with("oops") && !err.contains("line 5\n"), "{err}");

        let slow = script(dir.path(), "slow", "sleep 5");
        let spec = Spec { shown: "slow".into(), program: slow, args: vec![] };
        let err = run(&spec, &quick(dir.path(), Duration::from_secs(1))).await.unwrap_err();
        assert_eq!(err, "The preflight check `slow` didn't finish in 1 second.");

        let err = preflight("missing", &dir.path().join("missing"), &[], dir.path()).await.unwrap_err();
        assert!(err.starts_with("The preflight check `missing` can't start"), "{err}");
    }

    #[tokio::test]
    async fn shell_lines_words_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        script(bin.path(), "tool", "echo new tool");
        script(dir.path(), "install.sh", "echo \"$1 $ORRERY_VERSION\" > done");
        let env = [("ORRERY_VERSION".to_owned(), "v1.2.0".to_owned())];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Run {
            what: "install command",
            dir: dir.path(),
            bin: Some(bin.path()),
            env: &env,
            limit: COMMAND_LIMIT,
            out: Some(&tx),
        };

        // A shell line finds the new program first on PATH, and its output is passed on.
        run(&Command::Shell("tool && echo $ORRERY_VERSION".into()).spec(dir.path()), &r).await.unwrap();
        assert_eq!((rx.recv().await.unwrap(), rx.recv().await.unwrap()), ("new tool".into(), "v1.2.0".into()));

        // Words run without a shell; a word with a / is a path from the entry's folder.
        let words = Command::Words(vec!["./install.sh".into(), "--unattended".into()]);
        run(&words.spec(dir.path()), &r).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("done")).unwrap(), "--unattended v1.2.0\n");

        let err = run(&Command::Shell("echo half; exit 4".into()).spec(dir.path()), &r).await.unwrap_err();
        assert_eq!(err, "The install command `echo half; exit 4` failed with exit status 4:\nhalf");
        let r = Run { limit: Duration::from_secs(120), ..r };
        let err = run(&Command::Shell("sleep 0.1; exit 1".into()).spec(dir.path()), &r).await.unwrap_err();
        assert_eq!(err, "The install command `sleep 0.1; exit 1` failed with exit status 1.");
    }

    #[test]
    fn parses_the_forms() {
        let s = "update_manager tool";
        assert_eq!(parse_install(s, Some(&json!(true))).unwrap(), Some(Install::Bare));
        assert_eq!(parse_install(s, Some(&json!(false))).unwrap(), None);
        assert_eq!(parse_install(s, None).unwrap(), None);
        assert_eq!(
            parse_install(s, Some(&json!("./install.sh -y"))).unwrap(),
            Some(Install::Run(Command::Shell("./install.sh -y".into())))
        );
        assert_eq!(
            parse(s, "upgrade", Some(&json!(["./migrate.sh", "-q"]))).unwrap(),
            Some(Command::Words(vec!["./migrate.sh".into(), "-q".into()]))
        );
        // `true` is only for install: an upgrade always runs something.
        let err = parse(s, "upgrade", Some(&json!(true))).unwrap_err();
        assert_eq!(
            err,
            r#"[update_manager tool] upgrade: expected a shell line, such as "./upgrade.sh --quiet", or a list of words, such as ["./upgrade.sh", "--quiet"], got true"#
        );
        assert!(parse(s, "upgrade", Some(&json!([]))).is_err());
        assert!(parse(s, "upgrade", Some(&json!(""))).is_err());
        assert!(parse(s, "upgrade", Some(&json!([1]))).is_err());
    }
}
