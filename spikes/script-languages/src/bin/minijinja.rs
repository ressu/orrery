//! Reference only: Moonraker's own template text, to show what Jinja compatibility costs.
#[path = "../common.rs"]
mod common;

use common::Scenario;
use minijinja::{Environment, Value, context};

// Moonraker's double-click example calls `user_data.__setitem__`, a Python method that
// minijinja doesn't have; it would need a custom object, so it's left out here.
const SCENARIOS: &[Scenario] = &[
    Scenario { name: "estop", on_press: Some(r#"{% do call_method("printer.emergency_stop") %}"#), on_release: None },
    Scenario {
        name: "reboot on a long press",
        on_press: None,
        on_release: Some(r#"{% if event.elapsed_time > 1.0 %}{% do call_method("machine.reboot") %}{% endif %}"#),
    },
    Scenario {
        name: "gcode",
        on_press: Some(r#"{% do call_method("printer.gcode.script", script="PAUSE") %}"#),
        on_release: None,
    },
];

fn main() {
    let mut env = Environment::new();
    env.add_function("call_method", |method: String, kwargs: minijinja::value::Kwargs| {
        let mut params = serde_json::Map::new();
        for key in kwargs.args() {
            let v: Value = kwargs.get(key).unwrap();
            params.insert(key.to_owned(), serde_json::to_value(&v).unwrap_or_default());
        }
        Value::from(common::call_method(&method, params.into()).to_string())
    });
    let env: &'static Environment = Box::leak(Box::new(env));
    common::drive(SCENARIOS, || {
        Box::new(move |script, e| {
            env.render_str(script, context! { event => context! { pressed => e.pressed, elapsed_time => e.elapsed_time } })
                .map(|_| ())
                .map_err(|err| err.to_string())
        })
    });
}
