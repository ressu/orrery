//! Differential checks against responses recorded from upstream Moonraker.
//!
//! `--record` (reference target only) stores each run's call transcript as its *shape*: object
//! keys, value types and error codes, without the values themselves. Against orrery, a run
//! whose shapes differ from the recording fails, which catches missing, extra and retyped
//! fields even where a case's `expect` blocks don't look.
//!
//! Files: `conformance/snapshots/<area>/<case>.<transport>.json`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::case::{Case, Transport};
use crate::target::Paths;

pub fn path(paths: &Paths, case: &Case, transport: Transport) -> PathBuf {
    paths.conformance().join("snapshots").join(format!("{}.{}.json", case.id, transport.as_str()))
}

/// Reduces a transcript to its shape, with `ignore` paths (dotted, `*` matches any key or index)
/// replaced by `"any"`.
pub fn shape_of(transcript: &[Value], ignore: &[String]) -> Value {
    let patterns: Vec<Vec<&str>> = ignore.iter().map(|p| p.split('.').collect()).collect();
    Value::Array(
        transcript
            .iter()
            .map(|entry| {
                let mut out = Map::new();
                out.insert("step".into(), entry["step"].clone());
                out.insert("call".into(), entry["call"].clone());
                if let Some(r) = entry.get("result") {
                    out.insert("result".into(), shape(r, &mut Vec::new(), &patterns));
                }
                if let Some(e) = entry.get("error") {
                    out.insert("error".into(), json!({"code": e["code"]}));
                }
                Value::Object(out)
            })
            .collect(),
    )
}

fn shape(v: &Value, at: &mut Vec<String>, ignore: &[Vec<&str>]) -> Value {
    if ignore.iter().any(|p| p.len() == at.len() && p.iter().zip(at.iter()).all(|(p, a)| *p == "*" || p == a)) {
        return json!("any");
    }
    match v {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        Value::Array(items) => {
            // Arrays are summarized by their first element: lengths and per-element
            // differences belong in `expect` blocks.
            at.push("0".into());
            let first = items.first().map(|i| shape(i, at, ignore));
            at.pop();
            Value::Array(first.into_iter().collect())
        }
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    at.push(k.clone());
                    let s = shape(v, at, ignore);
                    at.pop();
                    (k.clone(), s)
                })
                .collect(),
        ),
    }
}

pub fn write(paths: &Paths, case: &Case, transport: Transport, shape: &Value) -> Result<()> {
    let path = path(paths, case, transport);
    std::fs::create_dir_all(path.parent().expect("snapshot path has a parent"))?;
    let mut text = serde_json::to_string_pretty(shape)?;
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

pub fn read(paths: &Paths, case: &Case, transport: Transport) -> Result<Option<Value>> {
    let path = path(paths, case, transport);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?))
}

/// Lists differences between a recorded transcript shape and an actual one, step by step, as
/// `step N (method) path: description`.
pub fn diff(recorded: &Value, actual: &Value) -> Vec<String> {
    let empty = Vec::new();
    let rec = recorded.as_array().unwrap_or(&empty);
    let act = actual.as_array().unwrap_or(&empty);
    let mut out = Vec::new();
    for (r, a) in rec.iter().zip(act) {
        let label = format!("step {} ({})", r["step"], r["call"].as_str().unwrap_or("?"));
        if r["call"] != a["call"] {
            out.push(format!("{label}: called {} instead; snapshot is stale, re-record it", a["call"]));
            continue;
        }
        let mut problems = Vec::new();
        diff_at(r, a, "$", &mut problems);
        out.extend(problems.into_iter().map(|p| format!("{label} {p}")));
    }
    if rec.len() != act.len() {
        out.push(format!("reference made {} calls, this run made {}", rec.len(), act.len()));
    }
    out
}

fn diff_at(rec: &Value, act: &Value, path: &str, out: &mut Vec<String>) {
    if rec == &json!("any") || act == &json!("any") {
        return;
    }
    match (rec, act) {
        (Value::Object(r), Value::Object(a)) => {
            for (k, rv) in r {
                match a.get(k) {
                    Some(av) => diff_at(rv, av, &format!("{path}.{k}"), out),
                    None => out.push(format!("{path}.{k}: missing (reference has {})", describe(rv))),
                }
            }
            for k in a.keys().filter(|k| !r.contains_key(*k)) {
                out.push(format!("{path}.{k}: not in reference"));
            }
        }
        (Value::Array(r), Value::Array(a)) => match (r.first(), a.first()) {
            (Some(rv), Some(av)) => diff_at(rv, av, &format!("{path}[0]"), out),
            (None, None) => {}
            // One side is empty: nothing to compare element-wise.
            _ => {}
        },
        (r, a) if r != a => out.push(format!("{path}: reference has {}, got {}", describe(r), describe(a))),
        _ => {}
    }
}

fn describe(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => "object".into(),
        Value::Array(_) => "array".into(),
        other => other.to_string(),
    }
}
