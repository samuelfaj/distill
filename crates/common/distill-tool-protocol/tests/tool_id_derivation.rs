// Modified for Distill by Samuel Fajreldines, 2026.
//! `ToolDescriptionWithSchema::derive_tool_id`: namespaced descriptions
//! render as `"{ns}:{name}"`; bare names pass through; descriptions whose
//! derived id fails [`ToolId`] validation return `Err`.

use distill_tool_protocol::{IdError, ToolDescriptionWithSchema, ToolId};
use distill_tool_types::ToolDescription;

fn entry(name: &str, namespace: Option<&str>) -> ToolDescriptionWithSchema {
    let mut description = ToolDescription::new(name, "test description");
    if let Some(ns) = namespace {
        description = description.with_namespace(ns);
    }
    ToolDescriptionWithSchema {
        description,
        input_schema: None,
        capabilities: None,
        notification_schemas: None,
    }
}

#[test]
fn bare_name_derives_to_tool_id_without_namespace() {
    let derived = entry("read_file", None).derive_tool_id().unwrap();
    assert_eq!(derived, ToolId::new("read_file").unwrap());
}

#[test]
fn namespaced_name_derives_to_namespaced_tool_id() {
    let derived = entry("read_file", Some("Distill"))
        .derive_tool_id()
        .unwrap();
    assert_eq!(derived, ToolId::new("Distill:read_file").unwrap());
}

#[test]
fn invalid_name_propagates_id_validation_error() {
    let err = entry("foo bar", None).derive_tool_id().unwrap_err();
    assert!(
        matches!(err, IdError::InvalidFormat { .. }),
        "expected InvalidFormat, got {err:?}"
    );
}

#[test]
fn invalid_namespace_propagates_id_validation_error() {
    let err = entry("read_file", Some("bad ns"))
        .derive_tool_id()
        .unwrap_err();
    assert!(matches!(err, IdError::InvalidFormat { .. }));
}

#[test]
fn empty_name_yields_empty_id_error() {
    let err = entry("", None).derive_tool_id().unwrap_err();
    assert_eq!(err, IdError::Empty);
}

#[test]
fn derivation_does_not_collide_across_namespaces() {
    let a = entry("read_file", Some("Distill"))
        .derive_tool_id()
        .unwrap();
    let b = entry("read_file", Some("github")).derive_tool_id().unwrap();
    let c = entry("read_file", None).derive_tool_id().unwrap();
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
}
