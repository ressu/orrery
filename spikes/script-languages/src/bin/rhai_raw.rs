#[path = "../common.rs"]
mod common;

use common::Scenario;
use rhai::{Dynamic, Engine, Map, Scope};

const SCENARIOS: &[Scenario] = &[
    Scenario { name: "estop", on_press: Some(r#"call_method("printer.emergency_stop")"#), on_release: None },
    Scenario {
        name: "reboot on a long press",
        on_press: None,
        on_release: Some(r#"if event.elapsed_time > 1.0 { call_method("machine.reboot") }"#),
    },
    Scenario {
        name: "double click",
        on_press: Some(
            r#"
            let clicked = user_data.clicked ?? false;
            user_data.clicked = true;
            if event.elapsed_time < 0.5 && clicked {
                user_data.clicked = false;
                send_notification(#{ action: "double_click" });
            }
            "#,
        ),
        on_release: None,
    },
    Scenario { name: "gcode", on_press: Some(r#"call_method("printer.gcode.script", #{ script: "PAUSE" })"#), on_release: None },
    Scenario { name: "runaway", on_press: Some("loop {}"), on_release: None },
];

fn json(d: Dynamic) -> serde_json::Value {
    rhai::serde::from_dynamic(&d).unwrap_or_default()
}

fn engine() -> Engine {
    let mut engine = Engine::new_raw();
    engine.set_max_operations(100_000);
    engine.register_fn("call_method", |m: &str| rhai::serde::to_dynamic(common::call_method(m, serde_json::json!({}))).unwrap());
    engine.register_fn("call_method", |m: &str, p: Map| {
        rhai::serde::to_dynamic(common::call_method(m, json(p.into()))).unwrap()
    });
    engine.register_fn("send_notification", |d: Dynamic| common::send_notification(json(d)));
    engine
}

fn main() {
    common::drive(SCENARIOS, || {
        let engine = engine();
        let mut scope = Scope::new();
        scope.push("user_data", Map::new());
        Box::new(move |script, e| {
            let mut event = Map::new();
            event.insert("pressed".into(), e.pressed.into());
            event.insert("elapsed_time".into(), e.elapsed_time.into());
            scope.set_value("event", event);
            // A real agent would compile each script once, at startup.
            engine.run_with_scope(&mut scope, script).map_err(|err| err.to_string())
        })
    });
}
