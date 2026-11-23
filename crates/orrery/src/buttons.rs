//! Buttons on the printer's own GPIO lines (docs/design/2026-11-23-buttons-and-actions.md).
//!
//! Orrery reads the `[button.<name>]` tables and gives them to the `orrery-gpio` agent, which
//! watches the lines and runs the buttons' actions. A `[button <name>]` section in
//! `moonraker.conf` converts when its templates are a single literal `call_method`; anything else
//! is left out with a warning, since converting it would need a Jinja parser.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::actions::{Action, Actions, Step};
use crate::auth::Role;
use crate::config::Config;

/// The events a button runs actions on.
pub const EVENTS: &[&str] = &["on_press", "on_release", "on_click", "on_long_press"];
/// Its other options.
const OPTIONS: &[&str] = &["pin", "debounce_period", "minimum_event_time", "long_press_time"];
/// The events Moonraker has. Its templates are what the conversion turns into actions.
const MOONRAKER_EVENTS: &[&str] = &["on_press", "on_release"];

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub name: String,
    /// The settings the agent gets: `pin` as text, the times as numbers, and each event as a
    /// list of action names.
    pub options: Map<String, Value>,
}

impl Button {
    /// The settings, with the action names that don't resolve left out, and `None` when no
    /// event has one left.
    pub fn resolved(&self, actions: &Actions) -> Option<Map<String, Value>> {
        let mut options = self.options.clone();
        let mut any = false;
        for event in EVENTS {
            let Some(Value::Array(names)) = options.get_mut(*event) else { continue };
            names.retain(|n| n.as_str().is_some_and(|n| actions.get(n).is_some()));
            any |= !names.is_empty();
        }
        any.then_some(options)
    }
}

/// The buttons in the config, the actions their moonraker.conf templates converted to, and
/// warnings for what can't be used.
pub fn from_config(config: &Config) -> (Vec<Button>, Vec<Action>, Vec<String>) {
    let mut buttons: Vec<Button> = Vec::new();
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    // `[button SKSM]` in moonraker.conf and `[button.sksm]` in orrery.toml are the same button,
    // with orrery.toml's options over moonraker.conf's, one at a time.
    let mut names: Vec<String> = Vec::new();
    for section in config.section_names("button ") {
        let name = section["button ".len()..].to_lowercase();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for name in names {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            warnings.push(format!("Button {name}: a button name may use letters, digits, '_' and '-'"));
            continue;
        }
        let sections: Vec<String> = config
            .section_names("button ")
            .into_iter()
            .filter(|s| s["button ".len()..].to_lowercase() == name)
            .collect();
        let mut from_toml = Map::new();
        let mut from_conf = Map::new();
        for section in &sections {
            let toml_keys = config.toml_keys(section);
            for (key, value) in config.section_json(section).unwrap_or_default() {
                if toml_keys.contains(&key) {
                    from_toml.insert(key, value);
                } else {
                    from_conf.insert(key, value);
                }
            }
        }
        let mut options = Map::new();
        for (key, value) in &from_conf {
            if from_toml.contains_key(key) {
                continue;
            }
            match key.as_str() {
                "pin" => {
                    options.insert(key.clone(), value.clone());
                }
                "debounce_period" | "minimum_event_time" => {
                    match value.as_str().and_then(|v| v.trim().parse::<f64>().ok()) {
                        Some(n) => {
                            options.insert(key.clone(), json!(n));
                        }
                        None => warnings.push(format!("Button {name}: {key}: {value} isn't a number of seconds")),
                    }
                }
                k if MOONRAKER_EVENTS.contains(&k) => {
                    let template = value.as_str().unwrap_or_default();
                    match convert(&name, k, template) {
                        Some(Converted::BuiltIn(action)) => {
                            options.insert(key.clone(), json!([action]));
                        }
                        Some(Converted::Action(action)) => {
                            options.insert(key.clone(), json!([action.name]));
                            actions.push(action);
                        }
                        None => warnings.push(format!(
                            "Button {name}: Orrery converts only a single call_method from moonraker.conf, so \
                             {key} is left out; set this button's actions in orrery.toml"
                        )),
                    }
                }
                // Moonraker's button has no other options.
                _ => {}
            }
        }
        for (key, value) in from_toml {
            match parse_option(&key, value) {
                Ok(value) => {
                    options.insert(key, value);
                }
                Err(e) => warnings.push(format!("[button.{name}] {e}")),
            }
        }
        if !options.contains_key("pin") {
            warnings.push(format!("[button.{name}] has no pin, so the button is left out"));
            continue;
        }
        if !EVENTS.iter().any(|e| options.contains_key(*e)) {
            warnings.push(format!("Button {name} has no actions, so it's left out"));
            continue;
        }
        buttons.push(Button { name, options });
    }
    (buttons, actions, warnings)
}

/// A setting from orrery.toml, checked.
fn parse_option(key: &str, value: Value) -> Result<Value, String> {
    match key {
        "pin" => match value {
            Value::String(_) => Ok(value),
            other => Err(format!("pin: expected text, such as \"^!gpio72\", got {other}")),
        },
        k if OPTIONS.contains(&k) => {
            let min = if k == "debounce_period" { 0.01 } else { 0.0 };
            match value.as_f64() {
                Some(n) if n >= min => Ok(json!(n)),
                _ => Err(format!("{k}: expected seconds, at least {min}, got {value}")),
            }
        }
        k if EVENTS.contains(&k) => match value {
            Value::String(name) => Ok(json!([name])),
            Value::Array(names) if names.iter().all(Value::is_string) => Ok(Value::Array(names)),
            other => Err(format!("{k}: expected a list of action names, such as [\"shutdown\"], got {other}")),
        },
        k => Err(format!("unknown option {k}; a button has {} and {}", OPTIONS.join(", "), EVENTS.join(", "))),
    }
}

/// Warnings for action names in the buttons that don't name an action.
pub fn check_names(buttons: &[Button], actions: &Actions) -> Vec<String> {
    let mut out = Vec::new();
    for b in buttons {
        for event in EVENTS {
            for name in b.options.get(*event).and_then(Value::as_array).into_iter().flatten() {
                let name = name.as_str().unwrap_or_default();
                if actions.get(name).is_none() {
                    out.push(format!("[button.{}] {event}: there's no action {name}", b.name));
                }
            }
        }
    }
    out
}

#[derive(Debug, PartialEq)]
enum Converted {
    BuiltIn(&'static str),
    Action(Action),
}

static CALL: LazyLock<Regex> = LazyLock::new(|| {
    let value = r#""[^"\\]*"|'[^'\\]*'|-?\d+(?:\.\d+)?|True|False|true|false"#;
    Regex::new(&format!(
        r#"^\{{%-?\s*do\s+call_method\(\s*(?:"([\w.]+)"|'([\w.]+)')((?:\s*,\s*[A-Za-z_]\w*\s*=\s*(?:{value}))*)\s*\)\s*-?%\}}$"#
    ))
    .expect("valid pattern")
});
static ARG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([A-Za-z_]\w*)\s*=\s*("[^"\\]*"|'[^'\\]*'|-?\d+(?:\.\d+)?|True|False|true|false)"#)
        .expect("valid pattern")
});

/// A template that's only `{% do call_method("<method>", key=literal, …) %}`, as an action,
/// or the built-in action it is. `None` for anything else.
fn convert(button: &str, option: &str, template: &str) -> Option<Converted> {
    let lines: Vec<&str> = template
        .lines()
        .map(str::trim)
        .filter(|l| !(l.is_empty() || l.starts_with("{#") && l.ends_with("#}")))
        .collect();
    let [line] = lines.as_slice() else { return None };
    let call = CALL.captures(line)?;
    let method = call.get(1).or_else(|| call.get(2))?.as_str();
    let mut params = Map::new();
    for arg in ARG.captures_iter(&call[3]) {
        let text = &arg[2];
        let value = match text {
            "True" | "true" => json!(true),
            "False" | "false" => json!(false),
            t if t.starts_with('"') || t.starts_with('\'') => json!(t[1..t.len() - 1]),
            t => serde_json::from_str(t).ok()?,
        };
        params.insert(arg[1].to_owned(), value);
    }
    if params.is_empty()
        && let Some(name) = crate::actions::built_in_for(method)
    {
        return Some(Converted::BuiltIn(name));
    }
    Some(Converted::Action(Action {
        agent: None,
        name: format!("{button}_{option}"),
        step: Step::Method { method: method.to_owned(), params },
        role: Role::Operator,
        from: Some(format!("Button {button}'s {option}, from moonraker.conf")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buttons(conf: &str, toml: &str) -> (Vec<Button>, Vec<Action>, Vec<String>) {
        let mut c = Config::parse(conf).unwrap();
        c.parse_toml(toml, std::path::Path::new("orrery.toml")).unwrap();
        from_config(&c)
    }

    #[test]
    fn pink0s_button_converts_to_the_built_in() {
        let conf = "[button SKSM]\npin: !gpio72\non_press:\n  {% do call_method(\"machine.shutdown\") %}\n";
        let (b, a, w) = buttons(conf, "");
        assert!(w.is_empty() && a.is_empty(), "{w:?} {a:?}");
        assert_eq!(
            b,
            [Button {
                name: "sksm".into(),
                options: json!({"pin": "!gpio72", "on_press": ["shutdown"]}).as_object().unwrap().clone()
            }]
        );
    }

    #[test]
    fn literal_arguments_make_an_action() {
        let t = r#"
          {# Toggle the printer's lights #}
          {% do call_method("printer.gcode.script", script='SET_PIN PIN=lights VALUE=1') %}
        "#;
        let Some(Converted::Action(a)) = convert("lights", "on_press", t) else { panic!() };
        assert_eq!(a.name, "lights_on_press");
        assert_eq!(
            a.step,
            Step::Method {
                method: "printer.gcode.script".into(),
                params: json!({"script": "SET_PIN PIN=lights VALUE=1"}).as_object().unwrap().clone()
            }
        );
        let Some(Converted::Action(a)) =
            convert("b", "on_press", r#"{% do call_method("m.x", n=-3, f=1.5, on=True, s="a") %}"#)
        else {
            panic!()
        };
        assert_eq!(
            a.step,
            Step::Method {
                method: "m.x".into(),
                params: json!({"n": -3, "f": 1.5, "on": true, "s": "a"}).as_object().unwrap().clone()
            }
        );
        assert_eq!(
            convert("b", "on_press", "{%- do call_method('printer.emergency_stop') -%}"),
            Some(Converted::BuiltIn("emergency_stop"))
        );
    }

    #[test]
    fn anything_else_is_left_out() {
        for t in [
            "{% if event.elapsed_time > 5.0 %}\n{% do call_method(\"machine.reboot\") %}\n{% endif %}",
            "{% do call_method(\"machine.reboot\") %}\n{% do call_method(\"machine.shutdown\") %}",
            "{% set d = 'printer' %}{% do call_method(\"machine.device_power.post_device\", device=d) %}",
            "{% do call_method(\"m\", device=user_data.x) %}",
            "{% do call_method(\"m\", params) %}",
            "{% do send_notification(\"pressed\") %}",
            "",
        ] {
            assert_eq!(convert("b", "on_press", t), None, "{t}");
        }
        let conf = "[button b]\npin: gpio1\non_press: {% if event.pressed %}{% endif %}\n";
        let (b, _, w) = buttons(conf, "");
        assert!(b.is_empty());
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].starts_with("Button b: Orrery converts only a single call_method"), "{w:?}");
    }

    #[test]
    fn orrery_toml_over_moonraker_conf() {
        let conf =
            "[button SKSM]\npin: !gpio72\ndebounce_period: .1\non_press: {% do call_method(\"machine.shutdown\") %}\n";
        let toml = "[button.sksm]\non_press = []\non_long_press = [\"shutdown\"]\nlong_press_time = 3\n";
        let (b, _, w) = buttons(conf, toml);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            Value::Object(b[0].options.clone()),
            json!({"pin": "!gpio72", "debounce_period": 0.1, "on_press": [], "on_long_press": ["shutdown"], "long_press_time": 3.0})
        );
    }

    #[test]
    fn bad_options_warn() {
        let toml = r#"
[button.a]
pin = 72
on_press = "shutdown"
debounce_period = 0.001
colour = "red"

[button.b]
pin = "gpio2"
"#;
        let (b, _, w) = buttons("", toml);
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(b.is_empty());
    }
}
