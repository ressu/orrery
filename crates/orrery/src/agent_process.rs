//! Agents orrery starts itself: the `command` of an `[agent.<name>]` table.
//!
//! Each agent runs in orrery's data folder, with `ORRERY_SOCKET` set to the unix socket and
//! `ORRERY_AGENT` to its name. Its output goes to orrery's log, one line at a time, marked with
//! its name. When it exits, orrery starts it again, waiting 1 second at first and twice as long
//! each time, up to a minute; the wait starts over once an agent has run for a minute. When
//! orrery stops, it stops its agents.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::server::App;

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(60);
/// An agent that ran this long is healthy, so a restart starts over at `FIRST_DELAY`.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// How long an agent gets to exit after SIGTERM, before SIGKILL.
const STOP_TIME: Duration = Duration::from_secs(5);
/// Longer output lines are cut, so one runaway line can't fill the log.
const MAX_LINE: usize = 4096;

/// The running agents. [`AgentProcesses::stop`] stops them.
pub struct AgentProcesses {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

/// Starts every agent with a `command`. Call it once the unix socket is open.
pub fn start(app: &Arc<App>, socket: Option<&Path>) -> AgentProcesses {
    let (stop, _) = watch::channel(false);
    let mut tasks = Vec::new();
    let commands: Vec<(String, Vec<String>)> =
        app.agents.commands().map(|(name, command)| (name.to_owned(), command.to_vec())).collect();
    if commands.is_empty() {
        return AgentProcesses { stop, tasks };
    }
    let Some(socket) = socket else {
        app.add_warning("orrery has no unix socket, so it can't start the agents in orrery.toml.");
        return AgentProcesses { stop, tasks };
    };
    for (name, command) in commands {
        let agent = Agent {
            app: app.clone(),
            name,
            command: home_command(command),
            socket: socket.to_owned(),
            stopping: stop.subscribe(),
        };
        tasks.push(tokio::spawn(agent.supervise()));
    }
    AgentProcesses { stop, tasks }
}

impl AgentProcesses {
    /// Stops every agent, and waits until they have exited.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

struct Agent {
    app: Arc<App>,
    name: String,
    command: Vec<String>,
    socket: PathBuf,
    stopping: watch::Receiver<bool>,
}

impl Agent {
    async fn supervise(self) {
        let mut stopping = self.stopping.clone();
        let mut delay = FIRST_DELAY;
        let mut warned = false;
        loop {
            let started = Instant::now();
            match self.spawn() {
                Ok(mut child) => {
                    tracing::info!("Started agent {} (process {})", self.name, child.id().unwrap_or_default());
                    let status = tokio::select! {
                        status = child.wait() => status,
                        () = stopped(&mut stopping) => {
                            self.terminate(&mut child).await;
                            return;
                        }
                    };
                    if started.elapsed() >= HEALTHY_RUN {
                        delay = FIRST_DELAY;
                    }
                    let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
                    tracing::warn!("Agent {} exited ({status}); starting it again in {} s", self.name, delay.as_secs());
                }
                Err(e) => {
                    let message = format!("orrery can't start the agent {}: {}: {e}", self.name, self.command[0]);
                    if warned {
                        tracing::warn!("{message}; trying again in {} s", delay.as_secs());
                    } else {
                        // Fluidd shows server.info's warnings.
                        self.app.add_warning(message);
                        warned = true;
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = stopped(&mut stopping) => return,
            }
            delay = (delay * 2).min(MAX_DELAY);
        }
    }

    fn spawn(&self) -> std::io::Result<Child> {
        let mut cmd = Command::new(&self.command[0]);
        cmd.args(&self.command[1..])
            .current_dir(&self.app.paths.data)
            .env("ORRERY_SOCKET", &self.socket)
            .env("ORRERY_AGENT", &self.name)
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Its own process group, so Ctrl-C in orrery's terminal reaches only orrery, which
            // then stops its agents in order.
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: the closure runs in the child between fork and exec, and makes only one system
        // call, prctl, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                // The agent gets SIGTERM if orrery dies without stopping it, such as with SIGKILL.
                // Otherwise the next orrery would start a second copy.
                rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        if let Some(out) = child.stdout.take() {
            tokio::spawn(log_lines(self.name.clone(), out));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(log_lines(self.name.clone(), err));
        }
        Ok(child)
    }

    /// SIGTERM, then SIGKILL if the agent hasn't exited in `STOP_TIME`.
    async fn terminate(&self, child: &mut Child) {
        if let Some(pid) = child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32)) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        match tokio::time::timeout(STOP_TIME, child.wait()).await {
            Ok(_) => tracing::info!("Stopped agent {}", self.name),
            Err(_) => {
                let _ = child.kill().await;
                tracing::warn!("Agent {} didn't stop in {} s, so orrery killed it", self.name, STOP_TIME.as_secs());
            }
        }
    }
}

/// Waits until orrery stops. The guard `wait_for` returns is dropped here, so the caller can
/// borrow the agent again.
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    drop(stopping.wait_for(|s| *s).await);
}

/// Copies an agent's output to orrery's log.
async fn log_lines(name: String, out: impl AsyncRead + Unpin) {
    let mut reader = BufReader::new(out);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut line = String::from_utf8_lossy(&buf).trim().to_owned();
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_LINE {
            let mut cut = MAX_LINE;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push_str(" …");
        }
        tracing::info!("[{name}] {line}");
    }
}

/// `~` at the start of the program's path is the home folder.
fn home_command(mut command: Vec<String>) -> Vec<String> {
    if let Ok(program) = crate::expand_home(Path::new(&command[0])) {
        command[0] = program.to_string_lossy().into_owned();
    }
    command
}
