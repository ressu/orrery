#[path = "../common.rs"]
mod common;

use std::sync::Arc;

use common::Scenario;
use rune::runtime::{Object, budget};
use rune::{Context, Module, Source, Sources, Value, Vm};

// Rune has no overloading, so `call_method` always takes params. Each script is a function body.
const SCENARIOS: &[Scenario] = &[
    Scenario { name: "estop", on_press: Some(r#"call_method("printer.emergency_stop", #{});"#), on_release: None },
    Scenario {
        name: "reboot on a long press",
        on_press: None,
        on_release: Some(r#"if event.elapsed_time > 1.0 { call_method("machine.reboot", #{}); }"#),
    },
    Scenario {
        name: "double click",
        on_press: Some(
            r#"
            let clicked = user_data.get("clicked").unwrap_or(false);
            user_data["clicked"] = true;
            if event.elapsed_time < 0.5 && clicked {
                user_data["clicked"] = false;
                send_notification(#{ action: "double_click" });
            }
            "#,
        ),
        on_release: None,
    },
    Scenario {
        name: "gcode",
        on_press: Some(r#"call_method("printer.gcode.script", #{ script: "PAUSE" });"#),
        on_release: None,
    },
    Scenario { name: "runaway", on_press: Some("loop {}"), on_release: None },
];

fn json(v: &Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or_default()
}

fn module() -> rune::support::Result<Module> {
    let mut m = Module::new();
    m.function("call_method", |method: &str, params: Value| {
        rune::to_value(common::call_method(method, json(&params)).to_string()).unwrap()
    })
    .build()?;
    m.function("send_notification", |data: Value| common::send_notification(json(&data))).build()?;
    Ok(m)
}

fn main() {
    let mut context = Context::with_default_modules().unwrap();
    context.install(module().unwrap()).unwrap();
    let runtime = Arc::new(context.runtime().unwrap());
    let context: &'static Context = Box::leak(Box::new(context));
    common::drive(SCENARIOS, || {
        let context = context;
        let runtime = runtime.clone();
        let user_data = rune::to_value(Object::new()).unwrap();
        let mut units = std::collections::HashMap::new();
        Box::new(move |script, e| {
            let unit = units.entry(script.to_owned()).or_insert_with(|| {
                let mut sources = Sources::new();
                let src = format!("pub fn main(event, user_data) {{ {script} }}");
                sources.insert(Source::memory(src).unwrap()).unwrap();
                Arc::new(rune::prepare(&mut sources).with_context(context).build().unwrap())
            });
            let mut event = Object::new();
            event.insert(rune::alloc::String::try_from("pressed").unwrap(), rune::to_value(e.pressed).unwrap()).unwrap();
            event.insert(rune::alloc::String::try_from("elapsed_time").unwrap(), rune::to_value(e.elapsed_time).unwrap()).unwrap();
            let mut vm = Vm::new(runtime.clone(), unit.clone());
            budget::with(100_000, || vm.call(["main"], (event, user_data.clone())))
                .call()
                .map(|_| ())
                .map_err(|err| err.to_string())
        })
    });
}
