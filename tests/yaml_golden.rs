//! The format layer must match js-yaml 5.4.3 byte for byte. The expected
//! output comes from `tests/fixtures/gen-yaml.mjs`, run against js-yaml.

use serde_json::Value as Json;
use tui_do_core::format::{Map, Value, dump_yaml, load_yaml};

fn fixtures() -> Json {
    serde_json::from_str(include_str!("fixtures/yaml-golden.json")).expect("valid fixture JSON")
}

fn decode(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Number(n.as_f64().expect("number")),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(items) => Value::Seq(items.iter().map(decode).collect()),
        Json::Object(obj) => {
            if let Some(ms) = obj.get("$date") {
                return Value::Date(ms.as_i64().expect("millis"));
            }
            if let Some(Json::String(n)) = obj.get("$num") {
                return Value::Number(match n.as_str() {
                    "NaN" => f64::NAN,
                    "Infinity" => f64::INFINITY,
                    "-Infinity" => f64::NEG_INFINITY,
                    _ => -0.0,
                });
            }
            Value::Map(obj.iter().map(|(k, v)| (k.clone(), decode(v))).collect())
        }
    }
}

fn as_map(v: Value) -> Map {
    match v {
        Value::Map(m) => m,
        other => panic!("expected a map, got {other:?}"),
    }
}

#[test]
fn dump_should_match_js_yaml() {
    let fx = fixtures();
    let mut failures = Vec::new();
    let cases = fx["dumps"].as_array().expect("dumps");
    for case in cases {
        let data = as_map(decode(&case["data"]));
        let expected = case["yaml"].as_str().expect("yaml");
        let actual = dump_yaml(&data);
        if actual != expected {
            failures.push(format!(
                "{}:\n  expected {expected:?}\n  actual   {actual:?}",
                case["name"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} dump cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn load_should_match_js_yaml() {
    let fx = fixtures();
    let mut failures = Vec::new();
    for case in fx["loads"].as_array().expect("loads") {
        let yaml = case["yaml"].as_str().expect("yaml");
        let expected = decode(&case["data"]);
        match load_yaml(yaml) {
            Ok(Some(actual)) if actual == expected => {}
            other => failures.push(format!(
                "{yaml:?}:\n  expected {expected:?}\n  actual   {other:?}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "load cases differ:\n{}",
        failures.join("\n")
    );
}
