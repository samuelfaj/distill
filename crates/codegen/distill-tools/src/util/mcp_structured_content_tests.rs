use super::render_structured_content;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const SUMMARY: &str = "7 product folders, 2 custom folders";

fn folders() -> Value {
    json!({"folders": [{"id": "p1", "name": "Alpha"}, {"id": "c1", "name": "Custom One"}]})
}

fn part(structured: Value, texts: &[&str]) -> Option<String> {
    render_structured_content(Some(&structured), texts.iter().copied())
}

#[test]
fn absent_or_null_structured_content_yields_nothing() {
    assert_eq!(None, render_structured_content(None, [SUMMARY]));
    assert_eq!(None, part(Value::Null, &[SUMMARY]));
}

#[test]
fn summary_only_content_gets_the_payload() {
    assert_eq!(Some(folders().to_string()), part(folders(), &[SUMMARY]));
}

/// Servers inline the copy in prose, a fence or its own block, in their serializer's format.
#[test]
fn json_inside_prose_or_a_fence_is_inlined() {
    let spaced =
        r#"{"folders": [{"id": "p1", "name": "Alpha"}, {"id": "c1", "name": "Custom One"}]}"#;
    let re_keyed = r#"{"folders":[{"name":"Alpha","id":"p1"},{"name":"Custom One","id":"c1"}]}"#;
    let indented = serde_json::to_string_pretty(&folders()).unwrap();
    let list = json!([{"id": "p1"}, {"id": "c1"}]);
    for (payload, json) in [
        (folders(), folders().to_string()),
        (folders(), spaced.to_owned()),
        (folders(), re_keyed.to_owned()),
        (folders(), indented),
        (list.clone(), list.to_string()),
    ] {
        assert_eq!(
            None,
            part(payload.clone(), &[&format!("{SUMMARY}: {json}")]),
            "prose: {json}"
        );
        assert_eq!(
            None,
            part(
                payload.clone(),
                &[&format!("Found 2 folders:\n```json\n{json}\n```")]
            ),
            "fence: {json}"
        );
        assert_eq!(None, part(payload, &[SUMMARY, &json]), "own block: {json}");
    }
}

/// A `[tag]` in the prose before an object copy must not hide it.
#[test]
fn brackets_of_the_other_kind_in_the_prose_do_not_hide_the_json() {
    let json = folders().to_string();
    assert_eq!(
        None,
        part(
            folders(),
            &[&format!("Step [1]: found [product] folders: {json}")]
        )
    );
}

/// Spec >= 2026-07-28 scalars: a string payload is carried as itself, so it is never sent twice.
#[test]
fn scalar_payload_as_the_whole_block_is_inlined() {
    let body = "# Report\n\nFolders [product] and {custom}: 9 in total.";
    for (payload, text) in [
        (json!(body), json!(body).to_string()),
        (json!(body), body.to_owned()),
        (json!(9), "9".to_owned()),
        (json!(true), "true\n".to_owned()),
    ] {
        assert_eq!(None, part(payload, &[SUMMARY, &text]), "{text}");
    }
    assert_eq!(Some("9".to_owned()), part(json!(9), &["9 folders"]));
}

#[test]
fn different_json_block_is_not_inlined() {
    assert_eq!(
        Some(folders().to_string()),
        part(folders(), &[r#"{"count":9}"#])
    );
}
