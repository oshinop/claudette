//! Protocol frames are open-ended JSON whose unknown fields must survive, so they stay `Value`s.

use serde_json::{Map, Value};

pub type Object = Map<String, Value>;

/// Parses one protocol line; `None` when it is not a JSON object.
pub fn parse_object(line: &str) -> Option<Object> {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

pub fn str_field<'a>(value: &'a Object, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A JSON string's text as it appears inside a serialized document (without the quotes).
pub fn json_text(s: &str) -> String {
    let quoted = Value::String(s.to_owned()).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}
