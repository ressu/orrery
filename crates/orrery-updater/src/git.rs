//! `git_repo` entries: what `git` says about a repo, read as Moonraker's `GitRepo` reads it
//! (`update_manager/git_deploy.py`, `refresh_repo_state`). orrery runs the `git` program; a git
//! library would add megabytes to the agent.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Channel, Entry, GitOptions};
use crate::version::GitVersion;

/// How long one `git` command may run. A fetch over a slow link takes the longest.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Moonraker lists at most this many commits behind, and keeps the first 30 and the tagged ones.
const MAX_LOG: usize = 100;
const KEEP_UNTAGGED: usize = 30;
/// Untracked files with these endings are reported, as Moonraker does: they may be code that
/// changes how the repo runs.
const SOURCE_ENDINGS: &[&str] = &[".py", ".c", ".cpp"];
const LOG_FORMAT: &str = "--format=sha:%H%x1Dauthor:%an%x1Ddate:%ct%x1Dsubject:%s%x1Dmessage:%b%x1E";
const REF_FORMAT: &str = "--format=%(if)%(*objecttype)%(then)%(*objecttype) %(*objectname)%(else)%(objecttype) %(objectname)%(end) %(refname)";

#[derive(Debug)]
pub struct GitError {
    pub command: String,
    /// The exit code; `None` when git didn't run or ran out of time.
    pub code: Option<i32>,
    pub message: String,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git {} failed: {}", self.command, self.message)
    }
}

/// Runs `git -C <path> <args>`, and returns what it printed.
pub async fn git(path: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError> {
    let command = args.join(" ");
    let fail = |code, message: String| GitError { command: command.clone(), code, message };
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        // English messages, and no prompt for a password that nobody can type.
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        // A stalled transfer gives up after 20 seconds below 1 kB/s, as Moonraker sets it.
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
        .env("GIT_HTTP_LOW_SPEED_TIME", "20")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(timeout, child).await {
        Err(_) => return Err(fail(None, format!("it ran for more than {} seconds", timeout.as_secs()))),
        Ok(Err(e)) => return Err(fail(None, e.to_string())),
        Ok(Ok(out)) => out,
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(fail(
            out.status.code(),
            if stderr.is_empty() { format!("exit status {}", out.status) } else { stderr },
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What a refresh found. `?` marks what couldn't be found, as in Moonraker.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoState {
    pub repo_detected: bool,
    pub branch: String,
    pub remote: String,
    pub url: String,
    pub recovery_url: String,
    pub owner: String,
    pub repo_name: String,
    pub current_commit: String,
    pub upstream_commit: String,
    pub current: GitVersion,
    pub upstream: GitVersion,
    pub commits_behind: Vec<Value>,
    pub commits_behind_count: u64,
    pub detached: bool,
    pub diverged: bool,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    pub pinned_valid: bool,
    pub warnings: Vec<String>,
    pub anomalies: Vec<String>,
}

impl RepoState {
    fn unknown() -> Self {
        Self {
            repo_detected: false,
            branch: "?".into(),
            remote: "?".into(),
            url: "?".into(),
            recovery_url: "?".into(),
            owner: "?".into(),
            repo_name: "?".into(),
            current_commit: "?".into(),
            upstream_commit: "?".into(),
            current: GitVersion::parse("?"),
            upstream: GitVersion::parse("?"),
            commits_behind: Vec::new(),
            commits_behind_count: 0,
            detached: false,
            diverged: false,
            modified: Vec::new(),
            untracked: Vec::new(),
            pinned_valid: true,
            warnings: Vec::new(),
            anomalies: Vec::new(),
        }
    }

    /// Moonraker's `GitRepo.is_valid`: a repo on a known branch and remote, with nothing a
    /// recover would have to fix.
    pub fn is_valid(&self) -> bool {
        self.repo_detected
            && self.branch != "?"
            && self.remote != "?"
            && self.upstream_commit != "?"
            && !self.diverged
            && !self.current.dirty
            && !self.detached
    }

    /// The entry's object in `version_info`, with Moonraker's fields.
    pub fn status(&self, entry: &Entry, rollback_version: Option<&str>) -> Value {
        let current = self.current.short();
        json!({
            "name": entry.name,
            "configured_type": "git_repo",
            "detected_type": "git_repo",
            "channel": entry.channel.as_str(),
            "debug_enabled": false,
            "channel_invalid": false,
            "is_valid": self.is_valid(),
            "info_tags": entry.info_tags,
            "repo_detected": self.repo_detected,
            "remote_alias": self.remote,
            "branch": self.branch,
            "owner": self.owner,
            "repo_name": self.repo_name,
            "remote_url": self.url,
            "recovery_url": self.recovery_url,
            "version": current,
            "remote_version": self.upstream.short(),
            "rollback_version": rollback_version.unwrap_or(&current),
            "current_hash": self.current_commit,
            "remote_hash": self.upstream_commit,
            "is_dirty": self.current.dirty,
            "detached": self.detached,
            "commits_behind": self.commits_behind,
            "commits_behind_count": self.commits_behind_count,
            "git_messages": [],
            "full_version_string": self.current.full,
            "pristine": self.untracked.is_empty() && !self.current.dirty,
            "corrupt": false,
            "warnings": self.warnings,
            "anomalies": if entry.report_anomalies { self.anomalies.clone() } else { Vec::new() },
        })
    }
}

/// What a detached repo tracked before, so a refresh can keep following it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tracking {
    pub branch: Option<String>,
    pub remote: Option<String>,
}

/// Reads the repo's state, and fetches from its remote first when `fetch` is set. A failed
/// fetch is a warning: the state then compares with what was fetched before.
pub async fn refresh(entry: &Entry, opts: &GitOptions, tracking: &Tracking, fetch: bool) -> RepoState {
    let path = opts.path.as_path();
    let mut s = RepoState::unknown();
    let run = |args: Vec<&str>| {
        let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            git(path, &args, COMMAND_TIMEOUT).await
        }
    };
    if !path.join(".git").exists() || run(vec!["rev-parse", "--git-dir"]).await.is_err() {
        s.warnings.push("No git repo detected at configured path".into());
        return s;
    }
    s.repo_detected = true;
    match run(vec!["status", "--porcelain", "-b"]).await {
        Ok(text) => (s.modified, s.untracked) = parse_status(&text),
        Err(e) => s.warnings.push(format!("Can't read the repo's status: {e}")),
    }

    // The branch, and the remote it tracks.
    match run(vec!["symbolic-ref", "--short", "-q", "HEAD"]).await {
        Ok(branch) => {
            s.branch = branch.trim().to_owned();
            let key = format!("branch.{}.remote", s.branch);
            s.remote =
                run(vec!["config", "--get", &key]).await.map(|r| r.trim().to_owned()).unwrap_or_else(|_| "?".into());
        }
        Err(_) => {
            s.detached = true;
            s.branch = tracking.branch.clone().unwrap_or_else(|| "?".into());
            s.remote = tracking.remote.clone().unwrap_or_else(|| "?".into());
        }
    }
    if s.remote != "?" {
        if let Ok(url) = run(vec!["remote", "get-url", &s.remote]).await {
            s.url = url.trim().to_owned();
        }
        if let Some(moved) = &opts.moved_origin
            && same_url(&s.url, moved)
        {
            match run(vec!["remote", "set-url", &s.remote, &opts.origin]).await {
                Ok(_) => {
                    tracing::info!(
                        "{}: the repo moved, so its remote {} now points at {}",
                        entry.name,
                        s.remote,
                        opts.origin
                    );
                    s.url = opts.origin.clone();
                }
                Err(e) => tracing::warn!("{}: the repo moved, but its remote can't be changed: {e}", entry.name),
            }
        }
    }
    s.recovery_url = if s.remote == "origin" {
        s.url.clone()
    } else {
        run(vec!["remote", "get-url", "origin"]).await.map(|u| u.trim().to_owned()).unwrap_or_else(|_| "?".into())
    };
    if fetch && s.remote != "?" {
        let remote = s.remote.clone();
        if let Err(e) = git(path, &["fetch", &remote, "--prune"], FETCH_TIMEOUT).await {
            crate::warn_first_line(&format!("{}: ", entry.name), &e.to_string());
            s.warnings.push(format!("Can't fetch from {remote}: {}", e.message));
        }
    }
    let tip = format!("{}/{}", s.remote, s.branch);
    let tip_known = s.remote != "?" && s.branch != "?";
    if tip_known && !s.detached {
        // `merge-base --is-ancestor` exits with 1 when HEAD isn't in the remote branch.
        s.diverged =
            matches!(run(vec!["merge-base", "--is-ancestor", "HEAD", &tip]).await, Err(GitError { code: Some(1), .. }));
    }
    (s.owner, s.repo_name) = owner_and_name(&s.url);
    s.current_commit = run(vec!["rev-parse", "HEAD"]).await.map(|c| c.trim().to_owned()).unwrap_or_else(|_| "?".into());
    let describe = run(vec!["describe", "--always", "--tags", "--long", "--dirty", "--abbrev=8"]).await;
    s.current = GitVersion::parse(describe.as_deref().unwrap_or("?"));

    // The version to update to.
    if tip_known {
        upstream(&mut s, entry, opts, &tip, &run).await;
    }

    if s.commits_behind_count > 0 && s.current_commit != s.upstream_commit {
        let to = if entry.channel == Channel::Dev && opts.pinned_commit.is_none() {
            tip.clone()
        } else {
            s.upstream_commit.clone()
        };
        let range = format!("{}..{to}", s.current_commit);
        let max = format!("--max-count={MAX_LOG}");
        let tags = tagged_commits(&tip, &run).await;
        if let Ok(log) = run(vec!["log", &range, LOG_FORMAT, &max]).await {
            for (i, mut commit) in parse_log(&log).into_iter().enumerate() {
                let tag =
                    commit["sha"].as_str().and_then(|sha| tags.iter().find(|(c, _)| c == sha)).map(|(_, t)| t.clone());
                if i < KEEP_UNTAGGED || tag.is_some() {
                    commit["tag"] = json!(tag);
                    s.commits_behind.push(commit);
                }
            }
        }
    }
    warnings(&mut s, opts);
    s
}

/// The upstream commit and version for the entry's channel, and how far behind HEAD is.
async fn upstream<F, Fut>(s: &mut RepoState, entry: &Entry, opts: &GitOptions, tip: &str, run: &F)
where
    F: Fn(Vec<&str>) -> Fut,
    Fut: Future<Output = Result<String, GitError>>,
{
    let describe = |r: String| {
        let r2 = r.clone();
        async move {
            run(vec!["describe", &r2, "--always", "--tags", "--long", "--abbrev=8"])
                .await
                .unwrap_or_else(|_| "?".into())
        }
    };
    let text;
    if let Some(pinned) = &opts.pinned_commit {
        s.upstream_commit = s.current_commit.clone();
        if !s.current_commit.to_ascii_lowercase().starts_with(pinned.as_str()) {
            let exists = run(vec!["cat-file", "-e", &format!("{pinned}^{{commit}}")]).await.is_ok();
            if !exists {
                s.pinned_valid = false;
            } else if run(vec!["merge-base", "--is-ancestor", &s.current_commit, pinned]).await.is_ok() {
                s.upstream_commit = pinned.clone();
            }
        }
        text = describe(s.upstream_commit.clone()).await;
    } else if entry.channel == Channel::Dev {
        s.upstream_commit =
            run(vec!["rev-parse", tip]).await.map(|c| c.trim().to_owned()).unwrap_or_else(|_| "?".into());
        text = describe(tip.to_owned()).await;
    } else {
        // The highest matching version: tags made in the same second tie on their dates.
        let mut found = ("?".to_owned(), "?".to_owned());
        let mut best: Option<GitVersion> = None;
        for (sha, tag) in tagged_commits(tip, run).await {
            let v = GitVersion::parse(&tag);
            let fits = match entry.channel {
                Channel::Stable => v.is_final_release(),
                _ => v.valid && !v.is_alpha(),
            };
            if fits && best.as_ref().is_none_or(|b| v.partial_cmp(b).is_some_and(|o| o.is_gt())) {
                found = (sha, tag);
                best = Some(v);
            }
        }
        s.upstream_commit = found.0;
        text = found.1;
    }
    if s.upstream_commit != "?" {
        let range = format!("HEAD..{}", s.upstream_commit);
        s.commits_behind_count =
            run(vec!["rev-list", &range, "--count"]).await.ok().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    }
    s.upstream = GitVersion::parse(&text);
    // On stable and beta, a tag that isn't newer than HEAD is no update.
    if entry.channel != Channel::Dev && opts.pinned_commit.is_none() {
        let newer = s.upstream.partial_cmp(&s.current).is_some_and(|o| o.is_gt());
        if !newer {
            s.upstream_commit = s.current_commit.clone();
            s.upstream = s.current.clone();
            s.commits_behind_count = 0;
        }
    }
}

/// Tags on the remote branch that contain HEAD, newest first: `(commit, tag)`.
async fn tagged_commits<F, Fut>(tip: &str, run: &F) -> Vec<(String, String)>
where
    F: Fn(Vec<&str>) -> Fut,
    Fut: Future<Output = Result<String, GitError>>,
{
    let merged = format!("--merged={tip}");
    let Ok(text) = run(vec![
        "for-each-ref",
        "--count=100",
        "--sort=-creatordate",
        "--contains=HEAD",
        &merged,
        REF_FORMAT,
        "refs/tags",
    ])
    .await
    else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some("commit"), Some(sha), Some(name), None) => {
                    Some((sha.to_owned(), name.rsplit('/').next().unwrap_or(name).to_owned()))
                }
                _ => None,
            }
        })
        .collect()
}

/// Moonraker's warnings, which make an entry invalid, and anomalies, which are only reported.
fn warnings(s: &mut RepoState, opts: &GitOptions) {
    if opts.pinned_commit.is_some() && !s.pinned_valid {
        s.anomalies.push(format!("Pinned Commit {} does not exist", opts.pinned_commit.as_deref().unwrap_or_default()));
    }
    if s.branch == "?" {
        s.warnings.push("Failed to detect git branch".into());
    } else if s.remote == "?" {
        s.warnings.push(format!("Failed to detect tracking remote for branch {}", s.branch));
    } else if s.branch != opts.primary_branch || s.remote != "origin" {
        s.anomalies.push(format!(
            "Repo not on official remote/branch, expected: origin/{}, detected: {}/{}",
            opts.primary_branch, s.remote, s.branch
        ));
    }
    if s.url == "?" {
        s.warnings.push("Failed to detect repo url".into());
    } else if !same_url(&s.url, &opts.origin) {
        s.anomalies.push(format!("Unofficial remote url: {}", s.url));
    }
    // An update over a connection anyone on the way can change (orrery's, not Moonraker's).
    if unencrypted(&s.url) {
        s.anomalies.push(format!("Unencrypted remote url: {}", s.url));
    }
    if !s.untracked.is_empty() {
        s.anomalies.push(format!("Repo has untracked source files: {}", s.untracked.join(", ")));
    }
    if s.diverged {
        s.anomalies.push("Repo has diverged from remote".into());
    }
    if s.detached {
        s.warnings.push("Detached HEAD detected".into());
    }
    if s.current.dirty {
        s.warnings.push(format!("Repo is dirty. Detected the following modified files: {}", s.modified.join(", ")));
    }
}

/// Modified files, and untracked source files, from `git status --porcelain -b`.
fn parse_status(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut modified, mut untracked) = (Vec::new(), Vec::new());
    for line in text.lines() {
        if line.len() < 4 || line.starts_with("##") {
            continue;
        }
        let (code, file) = line.split_at(2);
        let file = file.trim().to_owned();
        if code == "??" {
            if SOURCE_ENDINGS.iter().any(|e| file.ends_with(e)) {
                untracked.push(file);
            }
        } else if code.contains('M') {
            modified.push(file);
        }
    }
    (modified, untracked)
}

/// `git log` output in `LOG_FORMAT`: fields split by 0x1D, commits by 0x1E.
fn parse_log(text: &str) -> Vec<Value> {
    text.split('\u{1e}')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|commit| {
            let fields: serde_json::Map<String, Value> = commit
                .split('\u{1d}')
                .filter_map(|f| f.trim().split_once(':'))
                .map(|(k, v)| (k.to_owned(), json!(v.trim())))
                .collect();
            Value::Object(fields)
        })
        .collect()
}

/// The owner and the repo's name in an `https://host/owner/name.git` URL, as Moonraker reads
/// them; `?` for what isn't there.
fn owner_and_name(url: &str) -> (String, String) {
    let owner = url
        .split_once("://")
        .and_then(|(scheme, rest)| scheme.starts_with("http").then_some(rest))
        .and_then(|rest| rest.split('/').nth(1))
        .filter(|o| !o.is_empty())
        .unwrap_or("?");
    let name = url
        .rsplit('/')
        .next()
        .filter(|_| url.contains('/'))
        .map(|n| n.split('.').next().unwrap_or(n))
        .filter(|n| !n.is_empty())
        .unwrap_or("?");
    (owner.to_owned(), name.to_owned())
}

/// URLs compared as Moonraker compares them: without case, and with `.git` at the end.
/// The URL of the repo's default remote, read from its config, without the network.
pub async fn remote_url(path: &Path) -> Option<String> {
    git(path, &["ls-remote", "--get-url"], COMMAND_TIMEOUT)
        .await
        .ok()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
}

/// `http://` and `git://` remotes. HTTPS and SSH are encrypted, and a local path needs no
/// network.
fn unencrypted(url: &str) -> bool {
    let url = url.trim().to_ascii_lowercase();
    url.starts_with("http://") || url.starts_with("git://")
}

pub fn same_url(a: &str, b: &str) -> bool {
    let norm = |u: &str| {
        let u = u.trim().to_ascii_lowercase();
        if u.ends_with(".git") { u } else { format!("{u}.git") }
    };
    norm(a) == norm(b)
}

/// What tracking a state shows, to keep for the next refresh.
pub fn tracking_of(s: &RepoState) -> Tracking {
    let known = |v: &str| (v != "?").then(|| v.to_owned());
    Tracking { branch: known(&s.branch), remote: known(&s.remote) }
}

/// Where a rollback goes: the commit and branch before the last update, as Moonraker's
/// `capture_state_for_rollback` keeps them.
#[derive(Debug, Clone, PartialEq)]
pub struct Rollback {
    pub commit: String,
    /// The branch, or `<remote>/<branch>` for a detached repo; `?` when unknown.
    pub branch: String,
    pub version: String,
}

impl Rollback {
    pub fn of(s: &RepoState) -> Self {
        let branch = if !s.detached {
            s.branch.clone()
        } else if s.branch != "?" && s.remote != "?" {
            format!("{}/{}", s.remote, s.branch)
        } else {
            "?".into()
        };
        Self { commit: s.current_commit.clone(), branch, version: s.current.short() }
    }
}

/// Runs one git command for an update, with its error as text.
async fn step(path: &Path, args: &[&str]) -> Result<String, String> {
    git(path, args, FETCH_TIMEOUT).await.map_err(|e| e.to_string())
}

/// Moves the repo forward to `s.upstream_commit`, which a refresh just found. The repo must be
/// valid, so it's on a branch that hasn't diverged, and the move is a fast-forward.
pub async fn fast_forward(opts: &GitOptions, s: &RepoState) -> Result<(), String> {
    step(&opts.path, &["merge", "--ff-only", "-q", &s.upstream_commit]).await.map(drop)
}

/// The commit a soft recover resets to: the pinned commit or the current one, if the remote
/// branch has it, and the remote branch otherwise (Moonraker's `get_recovery_ref`).
async fn recovery_ref(path: &Path, upstream: &str, pinned: Option<&str>, current: &str) -> String {
    for commit in pinned.into_iter().chain((current != "?").then_some(current)) {
        if git(path, &["merge-base", "--is-ancestor", commit, upstream], COMMAND_TIMEOUT).await.is_ok() {
            return commit.to_owned();
        }
    }
    upstream.to_owned()
}

/// A soft recover: tracked files back as committed, on `primary_branch`, at the commit
/// [`recovery_ref`] picks. Untracked files stay.
pub async fn soft_recover(opts: &GitOptions, s: &RepoState) -> Result<(), String> {
    let path = opts.path.as_path();
    let key = format!("branch.{}.remote", opts.primary_branch);
    let remote = git(path, &["config", "--get", &key], COMMAND_TIMEOUT)
        .await
        .map_err(|_| format!("Failed to find remote for primary branch '{}'", opts.primary_branch))?;
    let upstream = format!("{}/{}", remote.trim(), opts.primary_branch);
    let to = recovery_ref(path, &upstream, opts.pinned_commit.as_deref(), &s.current_commit).await;
    if s.current.dirty {
        // Puts back the changed files. The reset below still runs if this fails.
        let _ = step(path, &["checkout", "--", "."]).await;
    }
    step(path, &["checkout", "-q", &opts.primary_branch]).await?;
    step(path, &["reset", "--hard", "-q", &to]).await.map(drop)
}

/// A hard recover: a fresh clone of `primary_branch` beside the repo, which then takes the
/// repo's place. The old repo is removed only once the new one is in place. HEAD goes back to
/// the commit the repo was at, when the clone has it.
pub async fn hard_recover(opts: &GitOptions, s: &RepoState) -> Result<(), String> {
    let path = opts.path.as_path();
    // A submodule or worktree has a `.git` file that points into another repo.
    if path.join(".git").is_file() {
        return Err(format!(
            "{} is a submodule or worktree of another git repo, so it can't be cloned again",
            path.display()
        ));
    }
    // A missing repo has no remote to read, so the configured origin is the one to clone.
    let url = if s.recovery_url == "?" { opts.origin.as_str() } else { s.recovery_url.as_str() };
    let (parent, base) = match (path.parent(), path.file_name()) {
        (Some(p), Some(b)) => (p, b.to_string_lossy()),
        _ => return Err(format!("{} can't be cloned again: it has no parent folder", path.display())),
    };
    let fresh = parent.join(format!(".{base}_repo_backup"));
    let old = parent.join(format!(".{base}_repo_old"));
    for leftover in [&fresh, &old] {
        if leftover.exists() {
            tokio::fs::remove_dir_all(leftover)
                .await
                .map_err(|e| format!("Can't remove {}: {e}", leftover.display()))?;
        }
    }
    let target = fresh.display().to_string();
    let clone = ["clone", "--branch", &opts.primary_branch, "--filter=blob:none", url, &target];
    if let Err(e) = git(parent, &clone, FETCH_TIMEOUT).await {
        let _ = tokio::fs::remove_dir_all(&fresh).await;
        return Err(format!("Git Clone Error: {}", e.message));
    }
    let had_repo = path.exists();
    if had_repo {
        tokio::fs::rename(path, &old).await.map_err(|e| format!("Can't move {} aside: {e}", path.display()))?;
    }
    if let Err(e) = tokio::fs::rename(&fresh, path).await {
        if had_repo {
            let _ = tokio::fs::rename(&old, path).await;
        }
        return Err(format!("Can't move the new clone into {}: {e}", path.display()));
    }
    if had_repo && let Err(e) = tokio::fs::remove_dir_all(&old).await {
        tracing::warn!("Can't remove the old repo at {}: {e}", old.display());
    }
    let to = recovery_ref(path, "HEAD", opts.pinned_commit.as_deref(), &s.current_commit).await;
    if to != "HEAD" {
        step(path, &["reset", "--hard", "-q", &to]).await?;
    }
    Ok(())
}

/// Goes back to `rb`. Returns false when the repo is there already.
pub async fn rollback(opts: &GitOptions, s: &RepoState, rb: &Rollback) -> Result<bool, String> {
    if rb.commit == "?" || rb.branch == "?" {
        return Err("Incomplete rollback data stored, cannot rollback".into());
    }
    let path = opts.path.as_path();
    if rb.branch != s.branch {
        step(path, &["checkout", "-q", &rb.branch]).await?;
    } else if rb.commit == s.current_commit {
        return Ok(false);
    }
    step(path, &["reset", "--hard", "-q", &rb.commit]).await?;
    Ok(true)
}

/// For tests and logs: the state's main facts in one line.
pub fn summary(s: &RepoState) -> String {
    if !s.repo_detected {
        return "no git repo".into();
    }
    match s.commits_behind_count {
        0 => format!("up to date at {}", s.current.short()),
        1 => format!("1 commit behind, at {}, {} available", s.current.short(), s.upstream.short()),
        n => format!("{n} commits behind, at {}, {} available", s.current.short(), s.upstream.short()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_status_lines() {
        let text = "## main...origin/main\n M klippy/toolhead.py\nMM docs/a.md\n?? klippy/extras/led_effect.py\n?? notes.txt\n";
        let (modified, untracked) = parse_status(text);
        assert_eq!(modified, ["klippy/toolhead.py", "docs/a.md"]);
        assert_eq!(untracked, ["klippy/extras/led_effect.py"]);
    }

    #[test]
    fn reads_urls() {
        let o = |u: &str| owner_and_name(u);
        assert_eq!(o("https://github.com/mainsail-crew/crowsnest.git"), ("mainsail-crew".into(), "crowsnest".into()));
        assert_eq!(o("https://github.com/Klipper3d/klipper"), ("Klipper3d".into(), "klipper".into()));
        assert_eq!(o("git@github.com:ressu/x.git"), ("?".into(), "x".into()));
        assert_eq!(o("?"), ("?".into(), "?".into()));
        assert!(same_url("https://GitHub.com/a/b", "https://github.com/a/b.git"));
    }

    #[test]
    fn reads_log() {
        let text = "sha:abc\u{1d}author:A B\u{1d}date:1700000000\u{1d}subject:Fix: things\u{1d}message:\u{1e}\nsha:def\u{1d}author:C\u{1d}date:1\u{1d}subject:s\u{1d}message:body\n\u{1e}";
        let log = parse_log(text);
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[0],
            json!({"sha": "abc", "author": "A B", "date": "1700000000", "subject": "Fix: things", "message": ""})
        );
        assert_eq!(log[1]["message"], "body");
    }

    /// A bare remote with commits and tags, and a clone of it on `main`.
    struct Repos {
        dir: tempfile::TempDir,
    }

    impl Repos {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let r = Self { dir };
            r.sh("git init -q --bare -b main origin.git");
            r.sh("git init -q -b main work && cd work && git remote add origin ../origin.git");
            r.commit("work", "one");
            r.sh("cd work && git tag v1.0.0 && git push -q origin main --tags");
            r.sh("git clone -q origin.git clone");
            r
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.dir.path().join(name)
        }

        fn sh(&self, script: &str) {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(self.dir.path())
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        }

        fn commit(&self, repo: &str, text: &str) {
            self.sh(&format!("cd {repo} && echo {text} >> file.txt && git add file.txt && git commit -q -m '{text}'"));
        }

        fn push(&self, text: &str) {
            self.commit("work", text);
            self.sh("cd work && git push -q origin main --tags");
        }

        fn entry(&self, channel: &str) -> (Entry, GitOptions) {
            let g = crate::config::global(&serde_json::Map::new(), &mut Vec::new());
            let origin = self.path("origin.git").display().to_string();
            let map = json!({"type": "git_repo", "path": self.path("clone"), "origin": origin, "primary_branch": "main", "channel": channel});
            let e = crate::config::entry("test", map.as_object().unwrap(), &g).unwrap();
            let crate::config::Kind::Git(opts) = e.kind.clone() else { panic!("a git entry") };
            (e, opts)
        }

        async fn refresh(&self, channel: &str) -> RepoState {
            let (e, opts) = self.entry(channel);
            refresh(&e, &opts, &Tracking::default(), true).await
        }
    }

    #[tokio::test]
    async fn up_to_date_and_behind() {
        let r = Repos::new();
        let s = r.refresh("dev").await;
        assert!(s.is_valid(), "{s:#?}");
        assert_eq!((s.branch.as_str(), s.remote.as_str(), s.commits_behind_count), ("main", "origin", 0));
        assert_eq!(s.current.short(), "v1.0.0-0");
        assert_eq!(s.current_commit, s.upstream_commit);
        assert!(s.anomalies.is_empty(), "{:?}", s.anomalies);

        r.push("two");
        r.push("three");
        let s = r.refresh("dev").await;
        assert_eq!((s.commits_behind_count, s.upstream.short()), (2, "v1.0.0-2".into()));
        let subjects: Vec<_> = s.commits_behind.iter().map(|c| c["subject"].as_str().unwrap()).collect();
        assert_eq!(subjects, ["three", "two"]);
        assert_eq!(s.commits_behind[0]["tag"], Value::Null);
        let (e, _) = r.entry("dev");
        let status = s.status(&e, None);
        assert_eq!(status["version"], "v1.0.0-0");
        assert_eq!(status["remote_version"], "v1.0.0-2");
        assert_eq!(status["rollback_version"], "v1.0.0-0");
        assert_eq!(status["is_dirty"], false);
    }

    #[tokio::test]
    async fn stable_follows_tags() {
        let r = Repos::new();
        r.push("two");
        let s = r.refresh("stable").await;
        // No newer release: up to date, though the branch moved on.
        assert_eq!((s.commits_behind_count, s.upstream.short()), (0, "v1.0.0-0".into()));
        r.sh("cd work && git tag v1.1.0-beta.1 && git push -q origin --tags");
        assert_eq!(r.refresh("stable").await.commits_behind_count, 0);
        let s = r.refresh("beta").await;
        assert_eq!((s.commits_behind_count, s.upstream.tag.as_str()), (1, "v1.1.0-beta.1"));
        assert_eq!(s.commits_behind[0]["tag"], "v1.1.0-beta.1");
        r.push("three");
        r.sh("cd work && git tag v1.1.0 && git push -q origin --tags");
        let s = r.refresh("stable").await;
        assert_eq!((s.commits_behind_count, s.upstream.tag.as_str()), (2, "v1.1.0"));
    }

    #[tokio::test]
    async fn dirty_untracked_and_diverged() {
        let r = Repos::new();
        r.sh("cd clone && echo change >> file.txt && touch extra.py notes.txt");
        let s = r.refresh("dev").await;
        assert!(s.current.dirty && !s.is_valid());
        assert_eq!(
            (s.modified.clone(), s.untracked.clone()),
            (vec!["file.txt".to_owned()], vec!["extra.py".to_owned()])
        );
        assert!(s.warnings.iter().any(|w| w.contains("Repo is dirty")), "{:?}", s.warnings);
        assert!(s.anomalies.iter().any(|a| a.contains("untracked source files: extra.py")), "{:?}", s.anomalies);

        // A local commit that the remote doesn't have.
        let r = Repos::new();
        r.push("upstream");
        r.commit("clone", "local");
        let s = r.refresh("dev").await;
        assert!(s.diverged && !s.is_valid());
        assert!(s.anomalies.iter().any(|a| a == "Repo has diverged from remote"));
    }

    #[tokio::test]
    async fn detached_and_missing() {
        let r = Repos::new();
        r.sh("cd clone && git checkout -q --detach HEAD");
        let (e, opts) = r.entry("dev");
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(s.detached && !s.is_valid());
        assert_eq!(s.branch, "?");
        // It keeps following what it tracked before.
        let tracking = Tracking { branch: Some("main".into()), remote: Some("origin".into()) };
        let s = refresh(&e, &opts, &tracking, false).await;
        assert_eq!((s.branch.as_str(), s.remote.as_str()), ("main", "origin"));
        assert!(s.warnings.iter().any(|w| w == "Detached HEAD detected"));

        let opts = GitOptions { path: r.path("nothing"), ..opts };
        let s = refresh(&e, &opts, &Tracking::default(), true).await;
        assert!(!s.repo_detected && !s.is_valid());
        assert_eq!(s.warnings, ["No git repo detected at configured path"]);
    }

    #[tokio::test]
    async fn update_and_roll_back() {
        let r = Repos::new();
        let (e, opts) = r.entry("dev");
        r.push("two");
        let before = r.refresh("dev").await;
        let rb = Rollback::of(&before);
        assert_eq!((rb.branch.as_str(), rb.version.as_str()), ("main", "v1.0.0-0"));
        fast_forward(&opts, &before).await.unwrap();
        let after = refresh(&e, &opts, &Tracking::default(), false).await;
        assert_eq!((after.current.short(), after.commits_behind_count), ("v1.0.0-1".into(), 0));
        assert!(rollback(&opts, &after, &rb).await.unwrap());
        let back = refresh(&e, &opts, &Tracking::default(), false).await;
        assert_eq!(back.current_commit, before.current_commit);
        // Already there.
        assert!(!rollback(&opts, &back, &rb).await.unwrap());
        let unknown = Rollback { commit: "?".into(), ..rb };
        assert!(rollback(&opts, &back, &unknown).await.is_err());
    }

    #[tokio::test]
    async fn soft_recover_resets_changes() {
        let r = Repos::new();
        let (e, opts) = r.entry("dev");
        r.sh("cd clone && echo change >> file.txt && touch extra.py && git checkout -q -b other");
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(s.current.dirty);
        soft_recover(&opts, &s).await.unwrap();
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(s.is_valid(), "{s:#?}");
        assert_eq!(s.branch, "main");
        // Untracked files stay.
        assert!(r.path("clone/extra.py").exists());

        // A diverged repo goes back to the remote branch, and its local commit is gone.
        r.push("upstream");
        r.commit("clone", "local");
        let s = refresh(&e, &opts, &Tracking::default(), true).await;
        assert!(s.diverged);
        soft_recover(&opts, &s).await.unwrap();
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(s.is_valid() && !s.diverged, "{s:#?}");
        assert_eq!(s.current.short(), "v1.0.0-1");
    }

    #[tokio::test]
    async fn hard_recover_clones_again() {
        let r = Repos::new();
        let (e, opts) = r.entry("dev");
        r.push("two");
        r.sh("cd clone && touch extra.py && echo change >> file.txt && git checkout -q --detach");
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(!s.is_valid());
        hard_recover(&opts, &s).await.unwrap();
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(s.is_valid(), "{s:#?}");
        assert!(!r.path("clone/extra.py").exists());
        assert!(!r.path(".clone_repo_backup").exists() && !r.path(".clone_repo_old").exists());

        // A missing repo is cloned from the configured origin, and a failed clone leaves the
        // repo as it was.
        r.sh("rm -rf clone");
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        hard_recover(&opts, &s).await.unwrap();
        assert!(refresh(&e, &opts, &Tracking::default(), false).await.is_valid());
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        let broken = RepoState { recovery_url: r.path("nothing.git").display().to_string(), ..s };
        assert!(hard_recover(&opts, &broken).await.unwrap_err().starts_with("Git Clone Error"));
        assert!(refresh(&e, &opts, &Tracking::default(), false).await.is_valid());
    }

    #[tokio::test]
    async fn hard_recover_keeps_the_version() {
        let r = Repos::new();
        let (e, opts) = r.entry("dev");
        r.push("two");
        // The clone is behind; a hard recover keeps it at its commit.
        let s = refresh(&e, &opts, &Tracking::default(), true).await;
        assert_eq!(s.commits_behind_count, 1);
        hard_recover(&opts, &s).await.unwrap();
        let after = refresh(&e, &opts, &Tracking::default(), false).await;
        assert_eq!(after.current_commit, s.current_commit);
    }

    #[tokio::test]
    async fn unofficial_remote_and_moved_origin() {
        let r = Repos::new();
        let (e, mut opts) = r.entry("dev");
        opts.origin = "https://github.com/someone/else.git".into();
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert!(s.anomalies.iter().any(|a| a.starts_with("Unofficial remote url:")), "{:?}", s.anomalies);
        // moved_origin names the old URL: orrery points the remote at origin.
        let (_, mut opts) = r.entry("dev");
        let real = opts.origin.clone();
        opts.moved_origin = Some(real.clone());
        opts.origin = r.path("origin-new.git").display().to_string();
        r.sh("cp -r origin.git origin-new.git");
        let s = refresh(&e, &opts, &Tracking::default(), false).await;
        assert_eq!(s.url, opts.origin);
        assert!(s.anomalies.is_empty(), "{:?}", s.anomalies);
    }

    #[test]
    fn unencrypted_remotes() {
        for url in ["http://github.com/a/b.git", "git://example.com/b", "HTTP://x/y"] {
            assert!(unencrypted(url), "{url}");
        }
        for url in ["https://github.com/a/b.git", "git@github.com:a/b.git", "ssh://git@x/y", "/home/printer/repo"] {
            assert!(!unencrypted(url), "{url}");
        }
    }
}
