//! The values a frontmatter block can hold, modeled on what js-yaml produces
//! in JavaScript: numbers are doubles, dates are millisecond timestamps, and
//! mapping keys are strings kept in insertion order.

use indexmap::IndexMap;

use super::js;

pub type Map = IndexMap<String, Value>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// Milliseconds since the epoch (a JS `Date`).
    Date(i64),
    Seq(Vec<Value>),
    Map(Map),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// `String(value)` in JS, as used when a non-string becomes a mapping key.
    pub fn to_js_string(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => js::number_to_string(*n),
            Value::String(s) => s.clone(),
            // JS would use Date.prototype.toString (local time); ISO is close
            // enough for the rare date-valued key.
            Value::Date(ms) => js::iso_string(*ms),
            Value::Seq(items) => items
                .iter()
                .map(Value::to_js_string)
                .collect::<Vec<_>>()
                .join(","),
            Value::Map(_) => "[object Object]".into(),
        }
    }

    /// JSON as `JSON.stringify` would produce it (dates become ISO strings).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => (*b).into(),
            // JSON.stringify writes integral numbers without a fraction
            Value::Number(n) if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 => {
                (*n as i64).into()
            }
            Value::Number(n) => {
                serde_json::Number::from_f64(*n).map_or(serde_json::Value::Null, Into::into)
            }
            Value::String(s) => s.clone().into(),
            Value::Date(ms) => js::iso_string(*ms).into(),
            Value::Seq(items) => items.iter().map(Value::to_json).collect(),
            Value::Map(map) => map_to_json(map),
        }
    }
}

pub fn map_to_json(map: &Map) -> serde_json::Value {
    serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.into())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Number(n as f64)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}
