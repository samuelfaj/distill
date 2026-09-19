pub fn try_extract_concatenated_json_objects(arguments: &str) -> Option<Vec<serde_json::Value>> {
    let trimmed = arguments.trim();

    if !trimmed.starts_with('{') {
        return None;
    }

    // If it parses as valid JSON already, no recovery needed.
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return None;
    }

    // The stream deserializer handles nested braces correctly (unlike naive string splitting on "}{")
    let stream = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();

    let mut objects = Vec::new();
    for result in stream {
        match result {
            Ok(value) if value.is_object() => objects.push(value),
            _ => break,
        }
    }

    // At least 2 objects are needed for this to be concatenated JSON
    if objects.len() >= 2 {
        Some(objects)
    } else {
        None
    }
}

/// Zero-arg MCP tools (e.g. `get_me`) sometimes receive `""` from the model instead of `"{}"`, which fails JSON parsing.
pub fn normalize_empty_arguments(arguments: &str) -> &str {
    if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_objects() {
        let args = r#"{"target_file": "a.java"}{"target_file": "b.java"}{"target_file": "c.java"}"#;
        let objects = try_extract_concatenated_json_objects(args).unwrap();
        assert_eq!(objects.len(), 3);
        assert_eq!(
            objects.first().and_then(|o| o.get("target_file")),
            Some(&serde_json::json!("a.java"))
        );
    }

    #[test]
    fn test_no_extract_for_valid_single_object() {
        assert!(
            try_extract_concatenated_json_objects(r#"{"target_file": "src/main.rs"}"#).is_none()
        );
    }

    #[test]
    fn test_no_extract_for_valid_object_with_braces_in_value() {
        assert!(
            try_extract_concatenated_json_objects(r#"{"command": "echo '}{' && ls"}"#).is_none()
        );
    }

    #[test]
    fn test_no_extract_for_array() {
        assert!(
            try_extract_concatenated_json_objects(
                r#"[{"target_file": "a.java"}, {"target_file": "b.java"}]"#
            )
            .is_none()
        );
    }

    #[test]
    fn test_no_extract_for_empty_or_non_json() {
        assert!(try_extract_concatenated_json_objects("").is_none());
        assert!(try_extract_concatenated_json_objects("not json").is_none());
    }

    #[test]
    fn test_extract_with_nested_braces() {
        let args = r#"{"file": "a.rs", "opts": {"line": 1}}{"file": "b.rs", "opts": {"line": 2}}"#;
        let objects = try_extract_concatenated_json_objects(args).unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(
            objects
                .first()
                .and_then(|o| o.get("opts"))
                .and_then(|o| o.get("line")),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn test_extract_with_whitespace_between_objects() {
        let objects = try_extract_concatenated_json_objects(r#"{"a": 1} {"b": 2}"#).unwrap();
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn test_extract_real_world_20_files() {
        let mut args = String::new();
        for i in 0..20 {
            args.push_str(&format!(r#"{{"target_file": "src/File{i}.java"}}"#));
        }
        let objects = try_extract_concatenated_json_objects(&args).unwrap();
        assert_eq!(objects.len(), 20);
    }

    #[test]
    fn test_no_extract_for_truncated_json() {
        assert!(try_extract_concatenated_json_objects(r#"{"a": 1} garbage"#).is_none());
    }

    /// Parse after normalizing; mirrors the production pattern in handle_tool_call.
    fn normalize_and_parse(arguments: &str) -> serde_json::Value {
        let normalized = normalize_empty_arguments(arguments);
        serde_json::from_str(normalized).unwrap_or_else(|_| serde_json::json!({"raw": arguments}))
    }

    #[test]
    fn empty_string_becomes_empty_object() {
        assert_eq!(normalize_and_parse(""), serde_json::json!({}));
    }

    #[test]
    fn whitespace_only_becomes_empty_object() {
        assert_eq!(normalize_and_parse("   "), serde_json::json!({}));
        assert_eq!(normalize_and_parse("\n\t"), serde_json::json!({}));
    }

    #[test]
    fn valid_json_unchanged() {
        assert_eq!(
            normalize_and_parse(r#"{"query": "test"}"#),
            serde_json::json!({"query": "test"})
        );
    }

    #[test]
    fn empty_object_string_unchanged() {
        assert_eq!(normalize_and_parse("{}"), serde_json::json!({}));
    }

    #[test]
    fn invalid_json_falls_back_to_raw() {
        let result = normalize_and_parse("not json");
        assert_eq!(result.get("raw"), Some(&serde_json::json!("not json")));
    }

    #[test]
    fn complex_args_with_arrays_unchanged() {
        let args = r#"{"pages": [{"title": "Test"}], "limit": 10}"#;
        let result = normalize_and_parse(args);
        assert!(result.get("pages").is_some_and(|p| p.is_array()));
        assert_eq!(result.get("limit"), Some(&serde_json::json!(10)));
    }

    #[test]
    fn normalize_empty_returns_braces() {
        assert_eq!(normalize_empty_arguments(""), "{}");
        assert_eq!(normalize_empty_arguments("   "), "{}");
        assert_eq!(normalize_empty_arguments("\n\t"), "{}");
    }

    #[test]
    fn normalize_non_empty_passthrough() {
        assert_eq!(normalize_empty_arguments(r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(normalize_empty_arguments("not json"), "not json");
    }
}
