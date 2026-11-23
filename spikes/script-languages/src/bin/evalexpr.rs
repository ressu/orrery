#[path = "../common.rs"]
mod common;

use common::Scenario;
use evalexpr::*;

// No maps: params are a tuple of (key, value) pairs. No `??`: state must exist first, so the
// harness sets `clicked = false` before the first run. `if` is a function.
const SCENARIOS: &[Scenario] = &[
    Scenario { name: "estop", on_press: Some(r#"call_method("printer.emergency_stop")"#), on_release: None },
    Scenario {
        name: "reboot on a long press",
        on_press: None,
        on_release: Some(r#"if(event_elapsed_time > 1.0, call_method("machine.reboot"), ())"#),
    },
    Scenario {
        name: "double click",
        on_press: Some(
            r#"
            double = event_elapsed_time < 0.5 && clicked;
            clicked = !double;
            if(double, send_notification("double_click"), ())
            "#,
        ),
        on_release: None,
    },
    Scenario {
        name: "gcode",
        on_press: Some(r#"call_method("printer.gcode.script", ("script", "PAUSE"))"#),
        on_release: None,
    },
];

fn json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => s.clone().into(),
        Value::Float(f) => (*f).into(),
        Value::Int(i) => (*i).into(),
        Value::Boolean(b) => (*b).into(),
        Value::Tuple(t) => t.iter().map(json).collect(),
        Value::Empty => serde_json::Value::Null,
    }
}

fn main() {
    common::drive(SCENARIOS, || {
        let mut ctx = HashMapContext::<DefaultNumericTypes>::new();
        ctx.set_function(
            "call_method".into(),
            Function::new(|arg| {
                let (method, params) = match arg {
                    Value::String(m) => (m.clone(), serde_json::json!({})),
                    Value::Tuple(t) => {
                        let pairs = t[1].as_tuple()?;
                        let mut map = serde_json::Map::new();
                        for kv in pairs.chunks(2) {
                            map.insert(kv[0].as_string()?, json(&kv[1]));
                        }
                        (t[0].as_string()?, map.into())
                    }
                    _ => return Err(EvalexprError::CustomMessage("call_method(method, params)".into())),
                };
                common::call_method(&method, params);
                Ok(Value::Empty)
            }),
        )
        .unwrap();
        ctx.set_function(
            "send_notification".into(),
            Function::new(|arg| {
                common::send_notification(json(arg));
                Ok(Value::Empty)
            }),
        )
        .unwrap();
        ctx.set_value("clicked".into(), false.into()).unwrap();
        Box::new(move |script, e| {
            ctx.set_value("event_pressed".into(), e.pressed.into()).map_err(|e| e.to_string())?;
            ctx.set_value("event_elapsed_time".into(), Value::from_float(e.elapsed_time)).map_err(|e| e.to_string())?;
            eval_empty_with_context_mut(script, &mut ctx).or_else(|err| match err {
                // The last expression isn't empty: fine.
                EvalexprError::ExpectedEmpty { .. } => Ok(()),
                err => Err(err),
            })
            .map_err(|err| err.to_string())
        })
    });
}
