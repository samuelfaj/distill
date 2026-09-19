use serde_json::json;

use super::ConversationKey;

#[test]
fn session_id_keys_a_request_ahead_of_its_first_system_message() {
    let cases = [
        (
            Some("session-a"),
            json!({ "messages": [
                { "role": "system", "content": "parent in plan mode" },
                { "role": "user", "content": "hello" }
            ] }),
            ConversationKey::SessionId("session-a".to_owned()),
        ),
        (
            None,
            json!({ "messages": [
                { "role": "system", "content": "parent" },
                { "role": "user", "content": "hello\nchild result" }
            ] }),
            ConversationKey::SystemMessage("parent".to_owned()),
        ),
        (
            None,
            json!({ "input": [{ "role": "user", "content": "summary?" }] }),
            ConversationKey::Unkeyed,
        ),
    ];
    for (session_id, body, expected) in cases {
        assert_eq!(
            expected,
            ConversationKey::from_request(session_id, &body),
            "{body}"
        );
    }
}
