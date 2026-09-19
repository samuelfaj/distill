use std::ffi::OsString;

use pretty_assertions::assert_eq;
use serde_json::json;

use super::{
    TOOL_DEFINITIONS_FILENAME, load_tool_definitions_from_dir, tool_definitions_bytes,
    tool_definitions_hash, write_tool_definitions_bytes,
};
use crate::sampling::ToolSpec;
use crate::sampling::types::ToolDefinition;

fn spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: Some(description.to_owned()),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn def(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition::function(
        name,
        Some(description),
        json!({"type": "object", "properties": {}}),
    )
}

#[test]
fn hash_is_stable_for_equal_specs_and_changes_with_any_field() {
    let specs = vec![spec("read_file", "Read a file."), spec("grep", "Search.")];
    let same = vec![spec("read_file", "Read a file."), spec("grep", "Search.")];
    let base = tool_definitions_hash(&specs);
    assert_eq!(base, tool_definitions_hash(&specs));
    assert_eq!(base, tool_definitions_hash(&same));

    let mut renamed = specs.clone();
    renamed.get_mut(1).expect("second spec").name = "rg".to_owned();
    assert_ne!(base, tool_definitions_hash(&renamed));

    let mut reworded = specs.clone();
    reworded.first_mut().expect("first spec").description =
        Some("Read a file with line numbers.".to_owned());
    assert_ne!(base, tool_definitions_hash(&reworded));

    let mut rescheme = specs.clone();
    rescheme.first_mut().expect("first spec").parameters =
        json!({"properties": {"path": {"type": "string"}}});
    assert_ne!(base, tool_definitions_hash(&rescheme));
}

#[test]
fn write_then_load_round_trips_in_wire_shape_and_leaves_only_the_artifact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let definitions = vec![
        def("read_file", "Read a file."),
        ToolDefinition::function("bare", None::<&str>, json!({})),
    ];
    let bytes = tool_definitions_bytes(&definitions).expect("definitions serialize");
    write_tool_definitions_bytes(dir.path(), &bytes).expect("write artifact");

    let expected = json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file.",
                "parameters": {"type": "object", "properties": {}}
            }
        },
        {"type": "function", "function": {"name": "bare", "parameters": {}}}
    ]);
    let bytes = std::fs::read(dir.path().join(TOOL_DEFINITIONS_FILENAME)).expect("read artifact");
    let on_disk: serde_json::Value = serde_json::from_slice(&bytes).expect("artifact is json");
    assert_eq!(expected, on_disk);
    let loaded = load_tool_definitions_from_dir(dir.path()).expect("artifact loads");
    let reloaded = serde_json::to_value(loaded).expect("definitions serialize");
    assert_eq!(expected, reloaded);

    let entries: Vec<OsString> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .map(|entry| entry.expect("dir entry").file_name())
        .collect();
    assert_eq!(vec![OsString::from(TOOL_DEFINITIONS_FILENAME)], entries);
}

#[test]
fn empty_toolset_writes_an_empty_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = tool_definitions_bytes(&[]).expect("empty toolset serializes");
    write_tool_definitions_bytes(dir.path(), &bytes).expect("write artifact");
    let loaded = load_tool_definitions_from_dir(dir.path()).expect("artifact loads");
    assert!(loaded.is_empty(), "{loaded:?}");
}

#[test]
fn load_reports_missing_and_malformed_artifacts_distinctly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = load_tool_definitions_from_dir(dir.path()).expect_err("nothing written yet");
    assert_eq!(std::io::ErrorKind::NotFound, missing.kind());

    std::fs::write(dir.path().join(TOOL_DEFINITIONS_FILENAME), b"[{").expect("write garbage");
    let malformed = load_tool_definitions_from_dir(dir.path()).expect_err("truncated json");
    assert_eq!(std::io::ErrorKind::InvalidData, malformed.kind());
}
