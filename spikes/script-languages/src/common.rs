//! What every candidate runs: Moonraker's button examples, fed the same edges.
#![allow(dead_code)]

use serde_json::{Value, json};

pub struct Event {
    pub pressed: bool,
    pub elapsed_time: f64,
}

/// A long hold, then a double click, then a lone click.
pub const EVENTS: &[Event] = &[
    Event { pressed: true, elapsed_time: 3.0 },
    Event { pressed: false, elapsed_time: 1.5 },
    Event { pressed: true, elapsed_time: 0.3 },
    Event { pressed: false, elapsed_time: 0.1 },
    Event { pressed: true, elapsed_time: 0.2 },
    Event { pressed: false, elapsed_time: 0.1 },
];

pub struct Scenario {
    pub name: &'static str,
    pub on_press: Option<&'static str>,
    pub on_release: Option<&'static str>,
}

/// Stands in for a call over Orrery's socket.
pub fn call_method(method: &str, params: Value) -> Value {
    println!("  call_method {method} {params}");
    json!("ok")
}

pub fn send_notification(data: Value) {
    println!("  send_notification {data}");
}

/// Runs each scenario over the events, with `run(script, event)` doing one event's script.
pub fn drive(scenarios: &[Scenario], mut new_button: impl FnMut() -> Box<dyn FnMut(&str, &Event) -> Result<(), String>>) {
    for s in scenarios {
        println!("{}:", s.name);
        let mut run = new_button();
        for e in EVENTS {
            let script = if e.pressed { s.on_press } else { s.on_release };
            if let Some(script) = script {
                if let Err(err) = run(script, e) {
                    println!("  error: {err}");
                    break;
                }
            }
        }
    }
}
