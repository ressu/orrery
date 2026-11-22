//! An entry's preflight: a check that the new version can run, before it takes effect
//! (docs/design/2026-11-22-self-update.md, "Preflight for every entry").

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// How long a check may run.
pub const LIMIT: Duration = Duration::from_secs(60);
/// How many lines of a failed check's output its message keeps.
const TAIL: usize = 10;

/// Runs `program` with `args` in `dir`. Exit status 0 passes. A failure's message names the
/// check as `name` and its arguments, and ends with the last lines of its output. `name` is
/// what the user knows the program as, since an update runs it from a hidden file.
pub async fn run(name: &str, program: &Path, args: &[String], dir: &Path, limit: Duration) -> Result<(), String> {
    let shown = std::iter::once(name.to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
    let spawn = || {
        tokio::process::Command::new(program)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
    };
    // A program written a moment ago can be "busy" while a process another thread started
    // still holds it open for writing; that ends when the process runs its own program.
    let mut tries = 0;
    let child = loop {
        match spawn() {
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::TXTBSY.raw_os_error()) && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => break result.map_err(|e| format!("The preflight check `{shown}` can't start: {e}"))?,
        }
    };
    let out = match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("The preflight check `{shown}` failed: {e}")),
        Err(_) => {
            let secs = limit.as_secs();
            let unit = if secs == 1 { "second" } else { "seconds" };
            return Err(format!("The preflight check `{shown}` didn't finish in {secs} {unit}."));
        }
    };
    if out.status.success() {
        return Ok(());
    }
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(TAIL)..].join("\n");
    let status = out.status.code().map_or_else(|| "a signal".to_owned(), |c| format!("exit status {c}"));
    Err(if tail.is_empty() {
        format!("The preflight check `{shown}` failed with {status}.")
    } else {
        format!("The preflight check `{shown}` failed with {status}:\n{tail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn passes_fails_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", "test \"$1\" = --check && test -f marker");
        std::fs::write(dir.path().join("marker"), "").unwrap();
        run("ok", &ok, &["--check".into()], dir.path(), LIMIT).await.unwrap();

        // An update runs the new program from a hidden file; the message names the program.
        let bad =
            script(dir.path(), ".tool.download", "for i in $(seq 1 15); do echo line $i; done; echo oops >&2; exit 3");
        let err = run("tool", &bad, &["--check".into()], dir.path(), LIMIT).await.unwrap_err();
        assert!(err.starts_with("The preflight check `tool --check` failed with exit status 3:\nline 7\n"), "{err}");
        assert!(err.ends_with("line 15\noops"), "{err}");

        let slow = script(dir.path(), "slow", "sleep 5");
        let err = run("slow", &slow, &[], dir.path(), Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(err, "The preflight check `slow` didn't finish in 1 second.");

        let err = run("missing", &dir.path().join("missing"), &[], dir.path(), LIMIT).await.unwrap_err();
        assert!(err.starts_with("The preflight check `missing` can't start"), "{err}");
    }
}
