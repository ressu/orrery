//! orrery-gpio: buttons on the printer's own GPIO lines, as an orrery agent
//! (docs/design/2026-11-23-buttons-and-actions.md).
//!
//! orrery starts it when a button is configured, and gives it the buttons with
//! `orrery.agent.config`, from the section `button`. For each, it watches the line, and when the
//! button is pressed, released, clicked, or held, runs that event's actions in order with
//! `server.actions.run`, stopping at the first that fails. One event at a time per button. Each
//! event that runs actions is sent as the agent's event `button_event`.

mod line;
mod pin;
mod press;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use orrery_agent::{Agent, AgentBuilder};
use serde_json::{Map, Value, json};

use press::{Fire, Press};

const NAME: &str = "gpio";

/// Buttons on the printer's own GPIO lines, as an orrery agent. orrery starts it.
#[derive(clap::Parser)]
#[command(version = orrery_build::LONG_VERSION)]
struct Cli {
    /// The unix socket to reach orrery at, for tests. orrery gives its agents their socket.
    socket: Option<String>,
}

#[derive(Debug)]
struct Button {
    name: String,
    pin: pin::Pin,
    debounce: Duration,
    minimum_event_time: Duration,
    long_press_time: Option<Duration>,
    /// The actions for each event.
    actions: Vec<(Fire, Vec<String>)>,
}

impl Button {
    fn parse(name: &str, options: &Map<String, Value>) -> Result<Self, String> {
        let pin = pin::parse(options.get("pin").and_then(Value::as_str).unwrap_or_default())?;
        let seconds = |key: &str, default: f64| {
            let n = options.get(key).and_then(Value::as_f64).unwrap_or(default);
            Duration::try_from_secs_f64(n).map_err(|_| format!("{key}: {n} isn't a number of seconds"))
        };
        let mut actions = Vec::new();
        for (fire, key) in [
            (Fire::Press, "on_press"),
            (Fire::Release, "on_release"),
            (Fire::Click, "on_click"),
            (Fire::LongPress, "on_long_press"),
        ] {
            let names: Vec<String> = options
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|n| n.as_str().map(str::to_owned))
                .collect();
            if !names.is_empty() {
                actions.push((fire, names));
            }
        }
        let tells_apart = actions.iter().any(|(f, _)| matches!(f, Fire::Click | Fire::LongPress));
        Ok(Self {
            name: name.to_owned(),
            pin,
            debounce: seconds("debounce_period", 0.05)?.max(Duration::from_millis(10)),
            minimum_event_time: seconds("minimum_event_time", 0.0)?,
            long_press_time: if tells_apart { Some(seconds("long_press_time", 1.0)?) } else { None },
            actions,
        })
    }

    fn actions(&self, fire: Fire) -> Option<&[String]> {
        self.actions.iter().find(|(f, _)| *f == fire).map(|(_, names)| names.as_slice())
    }
}

struct Gpio {
    /// The connection to orrery, when there is one.
    agent: Mutex<Option<Agent>>,
    /// Buttons that can't be used, for server.info. Set once, at the first connection.
    warnings: Mutex<Option<Vec<String>>>,
}

impl Gpio {
    fn agent(&self) -> Option<Agent> {
        self.agent.lock().expect("agent lock").clone()
    }

    /// Reads the buttons and starts watching their lines. Returns the warnings.
    async fn start(self: &Arc<Self>, agent: &Agent) -> Result<Vec<String>, orrery_agent::Error> {
        let mut warnings = Vec::new();
        for name in agent.config_names(Some("button")).await? {
            let Some(options) = agent.config(Some(&format!("button {name}"))).await? else { continue };
            let button = match Button::parse(&name, &options) {
                Ok(b) => b,
                Err(e) => {
                    warnings.push(format!("Button {name}: {e}"));
                    continue;
                }
            };
            let line = match line::Line::open(&button.pin, button.debounce) {
                Ok(line) => line,
                Err(e) => {
                    warnings.push(format!("Button {name}: can't open {}: {e}", button.pin));
                    continue;
                }
            };
            tracing::info!("Button {name}: watching {}", button.pin);
            let gpio = self.clone();
            tokio::spawn(async move { gpio.watch(button, line).await });
        }
        for w in &warnings {
            tracing::warn!("{w}");
        }
        Ok(warnings)
    }

    /// Runs a button's events, one at a time, until its line fails.
    async fn watch(self: Arc<Self>, button: Button, line: line::Line) {
        let active = match line.active() {
            Ok(active) => active,
            Err(e) => {
                tracing::warn!("Button {}: can't read {}: {e}", button.name, button.pin);
                false
            }
        };
        let mut press = Press::new(button.minimum_event_time, button.long_press_time, active);
        loop {
            let fires = match press.deadline() {
                Some(deadline) => {
                    let wait = deadline.saturating_sub(line::now());
                    tokio::select! {
                        edge = line.edge() => match edge {
                            Ok(edge) => press.edge(edge.active, edge.at),
                            Err(e) => break tracing::warn!("Button {}: can't read {}: {e}", button.name, button.pin),
                        },
                        () = tokio::time::sleep(wait) => press.timer(line::now()),
                    }
                }
                None => match line.edge().await {
                    Ok(edge) => press.edge(edge.active, edge.at),
                    Err(e) => break tracing::warn!("Button {}: can't read {}: {e}", button.name, button.pin),
                },
            };
            for fire in fires {
                if let Some(actions) = button.actions(fire) {
                    self.run(&button.name, fire, actions).await;
                }
            }
        }
    }

    /// Runs one event's actions in order, stopping at the first that fails.
    async fn run(&self, button: &str, fire: Fire, actions: &[String]) {
        let Some(agent) = self.agent() else {
            tracing::warn!("Button {button}: {}, but orrery isn't connected, so nothing runs", fire.name());
            return;
        };
        tracing::info!("Button {button}: {}, running {}", fire.name(), actions.join(", "));
        let event = json!({"name": button, "event": fire.name()});
        if let Err(e) = agent.send_event("button_event", Some(event)).await {
            tracing::debug!("Button {button}: can't send button_event: {e}");
        }
        for action in actions {
            if let Err(e) = agent.call("server.actions.run", json!({"action": action})).await {
                tracing::warn!("Button {button}: action {action} failed: {e}");
                return;
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = <Cli as clap::Parser>::parse();
    orrery_agent::init_logging();
    let gpio = Arc::new(Gpio { agent: Mutex::new(None), warnings: Mutex::new(None) });
    let mut builder = AgentBuilder::new(NAME).version(orrery_build::VERSION);
    if let Some(socket) = cli.socket {
        builder = builder.socket(socket);
    }
    orrery_agent::run(builder, move |agent| {
        let gpio = gpio.clone();
        async move {
            // The lines stay open across connections; orrery's buttons don't change while it
            // runs, and a new orrery starts a new agent.
            let known = gpio.warnings.lock().expect("warnings lock").clone();
            let warnings = match known {
                Some(w) => w,
                None => {
                    let w = gpio.start(&agent).await?;
                    *gpio.warnings.lock().expect("warnings lock") = Some(w.clone());
                    w
                }
            };
            agent.set_warnings(&warnings).await?;
            *gpio.agent.lock().expect("agent lock") = Some(agent);
            Ok(())
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let options = json!({"pin": "^!gpio23", "on_click": ["power.toggle"], "on_long_press": ["save", "shutdown"], "long_press_time": 2.0, "on_press": []});
        let b = Button::parse("power", options.as_object().unwrap()).unwrap();
        assert_eq!((b.pin.line, b.pin.inverted, b.pin.bias), (23, true, pin::Bias::PullUp));
        assert_eq!(
            (b.debounce, b.minimum_event_time, b.long_press_time),
            (Duration::from_millis(50), Duration::ZERO, Some(Duration::from_secs(2)))
        );
        assert_eq!(b.actions(Fire::LongPress), Some(&["save".to_owned(), "shutdown".to_owned()][..]));
        assert_eq!(b.actions(Fire::Press), None);

        // Without click or long press actions, a press is never told apart.
        let options = json!({"pin": "gpio72", "on_press": ["shutdown"], "debounce_period": 0.001});
        let b = Button::parse("sksm", options.as_object().unwrap()).unwrap();
        assert_eq!((b.long_press_time, b.debounce), (None, Duration::from_millis(10)));
        assert!(Button::parse("x", json!({"pin": "PA1"}).as_object().unwrap()).is_err());
    }
}
