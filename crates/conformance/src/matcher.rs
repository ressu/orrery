//! Partial JSON matching for `expect` blocks.
//!
//! An expected value matches an actual value when:
//! - objects: every expected key matches (extra actual keys are fine, unless `"$exact": true`);
//! - arrays: same length and every element matches, unless the expected array is wrapped as
//!   `{"$contains": [...]}`, which only needs each expected element to match some actual one;
//! - numbers: equal as `f64`, so `1` matches `1.0`;
//! - strings starting with `$` are patterns:
//!   - `$any`: anything, including null;
//!   - `$absent`: the key must not exist (only meaningful as an object value);
//!   - `$type:<t>`, with `t` one of `null`, `bool`, `number`, `string`, `array`, `object`;
//!   - `$regex:<re>`: a string matching `re`;
//!   - `$$...` matches the literal string `$...`;
//! - anything else: equal.

use regex::Regex;
use serde_json::Value;

/// Returns one line per mismatch, each naming its JSON path. Empty means the values match.
pub fn mismatches(expected: &Value, actual: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(expected, Some(actual), "$", &mut out);
    out
}

fn check(expected: &Value, actual: Option<&Value>, path: &str, out: &mut Vec<String>) {
    if let Value::String(pattern) = expected
        && let Some(rest) = pattern.strip_prefix('$')
        && !rest.starts_with('$')
    {
        check_pattern(rest, actual, path, out);
        return;
    }
    let Some(actual) = actual else {
        out.push(format!("{path}: missing, expected {}", short(expected)));
        return;
    };
    match (expected, actual) {
        (Value::Object(exp), _) if exp.contains_key("$contains") => {
            let Some(items) = exp["$contains"].as_array() else {
                out.push(format!("{path}: $contains needs an array"));
                return;
            };
            let Value::Array(act) = actual else {
                out.push(format!("{path}: expected an array, got {}", short(actual)));
                return;
            };
            for item in items {
                if !act.iter().any(|a| mismatches(item, a).is_empty()) {
                    out.push(format!("{path}: no element matches {}", short(item)));
                }
            }
        }
        (Value::Object(exp), Value::Object(act)) => {
            let exact = exp.get("$exact") == Some(&Value::Bool(true));
            for (k, v) in exp {
                if k == "$exact" {
                    continue;
                }
                check(v, act.get(k), &format!("{path}.{k}"), out);
            }
            if exact {
                for k in act.keys().filter(|k| !exp.contains_key(*k)) {
                    out.push(format!("{path}.{k}: unexpected key"));
                }
            }
        }
        (Value::Array(exp), Value::Array(act)) => {
            if exp.len() != act.len() {
                out.push(format!("{path}: expected {} elements, got {}: {}", exp.len(), act.len(), short(actual)));
                return;
            }
            for (i, (e, a)) in exp.iter().zip(act).enumerate() {
                check(e, Some(a), &format!("{path}[{i}]"), out);
            }
        }
        (Value::Number(e), Value::Number(a)) => {
            if e.as_f64() != a.as_f64() {
                out.push(format!("{path}: expected {e}, got {a}"));
            }
        }
        (Value::String(e), Value::String(a)) if e.starts_with("$$") => {
            if &e[1..] != a {
                out.push(format!("{path}: expected {:?}, got {a:?}", &e[1..]));
            }
        }
        (e, a) => {
            if e != a {
                out.push(format!("{path}: expected {}, got {}", short(e), short(a)));
            }
        }
    }
}

fn check_pattern(pattern: &str, actual: Option<&Value>, path: &str, out: &mut Vec<String>) {
    if pattern == "absent" {
        if let Some(a) = actual {
            out.push(format!("{path}: expected no value, got {}", short(a)));
        }
        return;
    }
    let Some(actual) = actual else {
        out.push(format!("{path}: missing, expected ${pattern}"));
        return;
    };
    if pattern == "any" {
        return;
    }
    if let Some(t) = pattern.strip_prefix("type:") {
        let ok = match t {
            "null" => actual.is_null(),
            "bool" => actual.is_boolean(),
            "number" => actual.is_number(),
            "string" => actual.is_string(),
            "array" => actual.is_array(),
            "object" => actual.is_object(),
            _ => {
                out.push(format!("{path}: unknown type {t:?} in pattern"));
                return;
            }
        };
        if !ok {
            out.push(format!("{path}: expected a {t}, got {}", short(actual)));
        }
        return;
    }
    if let Some(re) = pattern.strip_prefix("regex:") {
        match Regex::new(re) {
            Ok(re) => match actual.as_str() {
                Some(s) if re.is_match(s) => {}
                _ => out.push(format!("{path}: expected a string matching /{re}/, got {}", short(actual))),
            },
            Err(e) => out.push(format!("{path}: bad regex in pattern: {e}")),
        }
        return;
    }
    out.push(format!("{path}: unknown pattern ${pattern} (write $$ for a literal $)"));
}

/// A compact rendering of a value for error messages.
pub fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 120 { format!("{}...", s.chars().take(117).collect::<String>()) } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_objects_and_patterns() {
        let actual = json!({"a": 1.0, "b": "x-12", "c": [1, 2], "d": null});
        assert!(mismatches(&json!({"a": 1, "b": "$regex:^x-\\d+$", "c": "$type:array"}), &actual).is_empty());
        assert!(mismatches(&json!({"d": "$any", "e": "$absent"}), &actual).is_empty());
        assert_eq!(mismatches(&json!({"e": 1}), &actual), vec!["$.e: missing, expected 1"]);
        assert_eq!(mismatches(&json!({"a": 1, "$exact": true}), &json!({"a": 1, "z": 0})), vec!["$.z: unexpected key"]);
    }

    #[test]
    fn arrays() {
        assert!(!mismatches(&json!([1]), &json!([1, 2])).is_empty());
        assert!(mismatches(&json!({"$contains": [2]}), &json!([1, 2])).is_empty());
        assert!(!mismatches(&json!({"$contains": [3]}), &json!([1, 2])).is_empty());
    }

    #[test]
    fn literal_dollar() {
        assert!(mismatches(&json!("$$any"), &json!("$any")).is_empty());
        assert!(!mismatches(&json!("$$any"), &json!("other")).is_empty());
    }
}
