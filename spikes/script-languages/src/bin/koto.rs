#[path = "../common.rs"]
mod common;

use std::time::Duration;

use common::Scenario;
use koto::prelude::*;
use koto::serde::{from_koto_value, to_koto_value};

const SCENARIOS: &[Scenario] = &[
    Scenario { name: "estop", on_press: Some("call_method 'printer.emergency_stop'"), on_release: None },
    Scenario {
        name: "reboot on a long press",
        on_press: None,
        on_release: Some("if event.elapsed_time > 1.0 then call_method 'machine.reboot'"),
    },
    Scenario {
        name: "double click",
        on_press: Some(
            "
clicked = user_data.get 'clicked', false
user_data.clicked = true
if event.elapsed_time < 0.5 and clicked
  user_data.clicked = false
  send_notification {action: 'double_click'}
",
        ),
        on_release: None,
    },
    Scenario { name: "gcode", on_press: Some("call_method 'printer.gcode.script', {script: 'PAUSE'}"), on_release: None },
    Scenario { name: "runaway", on_press: Some("while true\n  x = 1"), on_release: None },
];

fn json(v: &KValue) -> serde_json::Value {
    from_koto_value(v.clone()).unwrap_or_default()
}

fn main() {
    common::drive(SCENARIOS, || {
        let mut koto = Koto::with_settings(KotoSettings::default().with_execution_limit(Duration::from_millis(100)));
        let prelude = koto.prelude();
        prelude.add_fn("call_method", |ctx| {
            let (method, params) = match ctx.args() {
                [KValue::Str(m)] => (m.to_string(), serde_json::json!({})),
                [KValue::Str(m), p] => (m.to_string(), json(p)),
                unexpected => return unexpected_args("|String|, or |String, Map|", unexpected),
            };
            Ok(to_koto_value(common::call_method(&method, params)).unwrap_or(KValue::Null))
        });
        prelude.add_fn("send_notification", |ctx| {
            common::send_notification(ctx.args().first().map(json).unwrap_or_default());
            Ok(KValue::Null)
        });
        prelude.insert("user_data", KMap::new());
        Box::new(move |script, e| {
            let event = KMap::new();
            event.insert("pressed", e.pressed);
            event.insert("elapsed_time", e.elapsed_time);
            koto.prelude().insert("event", event);
            koto.compile_and_run(script).map(|_| ()).map_err(|err| err.to_string())
        })
    });
}
