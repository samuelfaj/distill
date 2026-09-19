//! Lenient deserializers for tool arguments whose wire shape models get
//! wrong in predictable ways.
//!
//! Booleans may arrive as a JSON string (`"true"`) or number (`1`) when a
//! client doesn't coerce args against the tool schema. Accepted forms
//! (strings case-insensitive, trimmed; `null` is `false`):
//!
//! | Truthy                                | Falsy                                          |
//! |---------------------------------------|------------------------------------------------|
//! | `true`, `"true"`, `"yes"`, `"1"`, `1` | `false`, `"false"`, `"no"`, `"0"`, `0`, `null` |
//!
//! String lists (e.g. `task_ids`) may arrive as a bare string or number
//! instead of an array; see [`lenient_string_list_from_json`].

use serde::Deserialize;

const TRUE_LITERALS: [&str; 3] = ["true", "yes", "1"];
const FALSE_LITERALS: [&str; 3] = ["false", "no", "0"];

/// Parse a JSON value into a `bool` per the accepted forms; `None` otherwise.
pub fn lenient_bool_from_json(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Null => Some(false),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if TRUE_LITERALS
                .iter()
                .any(|lit| trimmed.eq_ignore_ascii_case(lit))
            {
                Some(true)
            } else if FALSE_LITERALS
                .iter()
                .any(|lit| trimmed.eq_ignore_ascii_case(lit))
            {
                Some(false)
            } else {
                None
            }
        }
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn invalid_bool_message(value: &serde_json::Value) -> String {
    format!(
        "expected a boolean (true/false, \"true\"/\"false\", \"yes\"/\"no\", \"1\"/\"0\", 1/0), got {value}"
    )
}

/// Deserialize a required `bool`; pair with `#[serde(default)]` so an absent key
/// uses the field default.
pub fn deserialize_lenient_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    lenient_bool_from_json(&value)
        .ok_or_else(|| serde::de::Error::custom(invalid_bool_message(&value)))
}

/// Deserialize `Option<bool>`: absent key → `None` (via `#[serde(default)]`),
/// explicit `null` → `Some(false)`.
pub fn deserialize_lenient_option_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    lenient_bool_from_json(&value)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom(invalid_bool_message(&value)))
}

/// Parse a JSON value into a list of strings, tolerating the shapes models
/// actually send:
///
/// - array of strings/numbers → each element as a string (`228` → `"228"`),
/// - bare string or number → one-element list,
/// - `null` → empty list.
///
/// Booleans, objects, and nested arrays are rejected (`None`).
pub fn lenient_string_list_from_json(value: &serde_json::Value) -> Option<Vec<String>> {
    fn item_to_string(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
    match value {
        serde_json::Value::Array(items) => items.iter().map(item_to_string).collect(),
        serde_json::Value::Null => Some(Vec::new()),
        other => item_to_string(other).map(|s| vec![s]),
    }
}

/// Deserialize a `Vec<String>` per [`lenient_string_list_from_json`]; pair
/// with `#[serde(default)]` so an absent key yields an empty list.
pub fn deserialize_lenient_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    lenient_string_list_from_json(&value).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "expected a list of string ids (or a single string), got {value}"
        ))
    })
}

const F64_EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

fn parse_lenient_whole_f64(f: f64) -> Result<i64, String> {
    if !f.is_finite() {
        return Err("expected finite number".into());
    }
    if f == 0.0 {
        return Ok(0);
    }
    if f.fract() != 0.0 {
        return Err(format!("expected whole number, got {f}"));
    }
    if f.abs() > F64_EXACT_INTEGER_LIMIT {
        return Err(format!(
            "number {f} exceeds f64 integer precision (whole floats above {F64_EXACT_INTEGER_LIMIT} may be inaccurate)"
        ));
    }
    Ok(f as i64)
}

/// Parse a JSON number or numeric string into a `u64`.
pub fn parse_lenient_u64_value(value: &serde_json::Value) -> Result<u64, String> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                return Ok(u);
            }
            if let Some(i) = n.as_i64() {
                if i < 0 {
                    return Err("expected non-negative number".into());
                }
                return u64::try_from(i).map_err(|_| "number out of range for u64".into());
            }
            if let Some(f) = n.as_f64() {
                let i = parse_lenient_whole_f64(f)?;
                return u64::try_from(i).map_err(|_| "expected non-negative number".to_string());
            }
            Err("expected number, got invalid numeric representation".into())
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if let Ok(u) = trimmed.parse::<u64>() {
                return Ok(u);
            }
            let i = parse_lenient_whole_f64(
                trimmed
                    .parse()
                    .map_err(|_| format!("expected number, got string \"{s}\""))?,
            )?;
            u64::try_from(i).map_err(|_| "expected non-negative number".to_string())
        }
        other => Err(format!("expected number, got {other}")),
    }
}

/// Parse a JSON number or numeric string into an `i64`.
pub fn parse_lenient_i64_value(value: &serde_json::Value) -> Result<i64, String> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(i);
            }
            if let Some(u) = n.as_u64() {
                return i64::try_from(u).map_err(|_| "number out of range for i64".into());
            }
            if let Some(f) = n.as_f64() {
                return parse_lenient_whole_f64(f);
            }
            Err("expected number, got invalid numeric representation".into())
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if let Ok(i) = trimmed.parse::<i64>() {
                return Ok(i);
            }
            parse_lenient_whole_f64(
                trimmed
                    .parse()
                    .map_err(|_| format!("expected number, got string \"{s}\""))?,
            )
        }
        other => Err(format!("expected number, got {other}")),
    }
}

pub fn deserialize_lenient_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => parse_lenient_i64_value(&v)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

pub fn deserialize_lenient_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => parse_lenient_u64_value(&v)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

pub fn deserialize_lenient_required_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    parse_lenient_u64_value(&value).map_err(serde::de::Error::custom)
}

pub fn deserialize_lenient_required_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    parse_lenient_i64_value(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_string_true_false() {
        assert_eq!(lenient_bool_from_json(&json!("true")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("false")), Some(false));
    }

    #[test]
    fn parses_yes_no() {
        assert_eq!(lenient_bool_from_json(&json!("yes")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("no")), Some(false));
    }

    #[test]
    fn parses_string_one_zero() {
        assert_eq!(lenient_bool_from_json(&json!("1")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("0")), Some(false));
    }

    #[test]
    fn parses_numeric_one_zero() {
        assert_eq!(lenient_bool_from_json(&json!(1)), Some(true));
        assert_eq!(lenient_bool_from_json(&json!(0)), Some(false));
    }

    #[test]
    fn is_case_insensitive_and_trims() {
        assert_eq!(lenient_bool_from_json(&json!("TRUE")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("False")), Some(false));
        assert_eq!(lenient_bool_from_json(&json!("  yes  ")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("No")), Some(false));
    }

    #[test]
    fn parses_null_as_false() {
        assert_eq!(lenient_bool_from_json(&json!(null)), Some(false));
    }

    #[test]
    fn rejects_unknown_forms() {
        for v in [
            json!("maybe"),
            json!(""),
            json!(2),
            json!(-1),
            json!(1.5),
            json!(1.0),
            json!([]),
            json!({}),
        ] {
            assert_eq!(lenient_bool_from_json(&v), None, "should reject {v}");
        }
    }

    fn deser_bool(json_str: &str) -> Result<bool, serde_json::Error> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_lenient_bool")]
            value: bool,
        }
        Ok(serde_json::from_str::<Wrapper>(json_str)?.value)
    }

    fn deser_opt_bool(json_str: &str) -> Result<Option<bool>, serde_json::Error> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_lenient_option_bool")]
            value: Option<bool>,
        }
        Ok(serde_json::from_str::<Wrapper>(json_str)?.value)
    }

    #[test]
    fn required_accepts_all_forms() {
        assert!(deser_bool(r#"{"value":true}"#).unwrap());
        assert!(deser_bool(r#"{"value":"true"}"#).unwrap());
        assert!(deser_bool(r#"{"value":"yes"}"#).unwrap());
        assert!(deser_bool(r#"{"value":"1"}"#).unwrap());
        assert!(deser_bool(r#"{"value":1}"#).unwrap());
        assert!(!deser_bool(r#"{"value":"no"}"#).unwrap());
        assert!(!deser_bool(r#"{"value":0}"#).unwrap());
    }

    #[test]
    fn required_missing_uses_default() {
        assert!(!deser_bool(r#"{}"#).unwrap());
    }

    #[test]
    fn required_null_is_false() {
        assert!(!deser_bool(r#"{"value":null}"#).unwrap());
    }

    #[test]
    fn required_rejects_unknown() {
        let err = deser_bool(r#"{"value":"maybe"}"#).unwrap_err();
        assert!(err.to_string().contains("expected a boolean"));
    }

    #[test]
    fn optional_missing_is_none_but_null_is_false() {
        assert_eq!(deser_opt_bool(r#"{}"#).unwrap(), None);
        assert_eq!(deser_opt_bool(r#"{"value":null}"#).unwrap(), Some(false));
    }

    #[test]
    fn optional_parses_and_rejects() {
        assert_eq!(deser_opt_bool(r#"{"value":"yes"}"#).unwrap(), Some(true));
        assert_eq!(deser_opt_bool(r#"{"value":0}"#).unwrap(), Some(false));
        assert!(deser_opt_bool(r#"{"value":"nope"}"#).is_err());
    }

    // ── lenient string lists ─────────────────────────────────────────────

    #[test]
    fn string_list_accepts_arrays_strings_and_numbers() {
        assert_eq!(
            lenient_string_list_from_json(&json!(["a", "b"])),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            lenient_string_list_from_json(&json!("abc")),
            Some(vec!["abc".to_string()])
        );
        // A bare OS-PID-style number becomes a one-element string list so the
        // tool can answer with a clean "Task 228 not found" instead of a
        // deserialize error.
        assert_eq!(
            lenient_string_list_from_json(&json!(228)),
            Some(vec!["228".to_string()])
        );
        assert_eq!(
            lenient_string_list_from_json(&json!([1, "b"])),
            Some(vec!["1".to_string(), "b".to_string()])
        );
        assert_eq!(lenient_string_list_from_json(&json!(null)), Some(vec![]));
    }

    #[test]
    fn string_list_rejects_non_id_shapes() {
        for v in [json!(true), json!({}), json!([["nested"]]), json!([true])] {
            assert_eq!(lenient_string_list_from_json(&v), None, "should reject {v}");
        }
    }

    #[test]
    fn deserialize_string_list_reports_readable_error() {
        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_lenient_string_list")]
            value: Vec<String>,
        }
        let err = serde_json::from_str::<Wrapper>(r#"{"value":{}}"#).unwrap_err();
        assert!(err.to_string().contains("expected a list of string ids"));
        let ok: Wrapper = serde_json::from_str(r#"{}"#).unwrap();
        assert!(ok.value.is_empty());
    }
}
