//! Typed access to the JSON hook payload Codex sends on stdin.

use serde_json::{Map, Value};

use crate::HookResult;

/// Parse the hook payload, which Codex always sends as a JSON object.
///
/// Empty or whitespace-only input maps to an empty object, because some events
/// carry no payload.
///
/// # Errors
///
/// Returns an error if the input is not valid JSON, or if it is valid JSON of
/// any kind other than an object. A non-object payload is rejected rather than
/// silently treated as empty, so malformed input fails explicitly.
pub(crate) fn parse_json_object(text: &str) -> HookResult<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| format!("failed to parse hook JSON: {error}"))?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(format!(
            "hook payload must be a JSON object, got {}",
            json_kind(&parsed)
        ))
    }
}

/// Human-readable name of a JSON value's kind, for error messages.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Return the first present, non-empty string among `keys`.
pub(crate) fn read_field_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| read_string(value.get(*key)))
}

/// Borrow `value` as a JSON object, if it is one.
pub(crate) fn read_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// Read a string value, treating whitespace-only strings as absent.
pub(crate) fn read_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_accepts_an_object() {
        let value = parse_json_object(r#"{"a":1}"#).expect("object should parse");
        assert_eq!(value.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn json_object_treats_blank_input_as_an_empty_object() {
        let value = parse_json_object("   ").expect("blank input should parse");
        assert!(
            value.as_object().is_some_and(Map::is_empty),
            "expected empty object, got {value}"
        );
    }

    #[test]
    fn json_object_rejects_a_non_object_payload() {
        let error = parse_json_object("[1,2]").unwrap_err();
        assert!(
            error.contains("must be a JSON object"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn json_object_rejects_invalid_json() {
        let error = parse_json_object("{not json").unwrap_err();
        assert!(
            error.contains("failed to parse hook JSON"),
            "unexpected error: {error}"
        );
    }
}
