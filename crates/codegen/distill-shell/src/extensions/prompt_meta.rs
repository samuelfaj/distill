use serde::{Deserialize, Serialize};

/// Typed metadata for a prompt `TextContent._meta` field.
/// Use this instead of ad-hoc `serde_json::json!()` on the sender side and manual `.get()` parsing on the receiver side.
/// Wire-compatible with the existing format: `{"bash_command": "ls -la"}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBlockMeta {
    /// Direct bash command to execute (bypasses agent loop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash_command: Option<String>,
}

impl PromptBlockMeta {
    /// Create meta for a direct bash command.
    pub fn bash(command: impl Into<String>) -> Self {
        Self {
            bash_command: Some(command.into()),
        }
    }

    /// Try to parse from a freeform `_meta` map.
    pub fn from_value(value: &agent_client_protocol::Meta) -> Option<Self> {
        serde_json::from_value(serde_json::Value::Object(value.clone())).ok()
    }

    /// The first text block that carries a `bash_command`. Other `_meta` is skipped.
    pub fn command_in(blocks: &[agent_client_protocol::ContentBlock]) -> Option<String> {
        blocks.iter().find_map(|block| {
            let agent_client_protocol::ContentBlock::Text(text) = block else {
                return None;
            };
            Self::from_value(text.meta.as_ref()?)?.bash_command
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_roundtrip_serde() {
        let meta = PromptBlockMeta::bash("ls -la");
        let json = serde_json::to_value(&meta).unwrap();
        let parsed: PromptBlockMeta = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.bash_command, Some("ls -la".to_string()));
    }

    #[test]
    fn from_value_legacy_compat() {
        let val = serde_json::json!({"bash_command": "ls"});
        let meta = PromptBlockMeta::from_value(val.as_object().unwrap()).unwrap();
        assert_eq!(meta.bash_command, Some("ls".to_string()));
    }

    #[test]
    fn from_value_unrelated_meta() {
        let val = serde_json::json!({"other": 1});
        let meta = PromptBlockMeta::from_value(val.as_object().unwrap());
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().bash_command, None);
    }

    #[test]
    fn from_value_empty_object() {
        let val = serde_json::json!({});
        let meta = PromptBlockMeta::from_value(val.as_object().unwrap());
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().bash_command, None);
    }

    #[test]
    fn skip_serializing_none() {
        let meta = PromptBlockMeta { bash_command: None };
        let json = serde_json::to_value(&meta).unwrap();
        assert!(!json.as_object().unwrap().contains_key("bash_command"));
    }

    #[test]
    fn command_in_reads_the_first_bash_meta() {
        let meta = serde_json::to_value(PromptBlockMeta::bash("ls -la"))
            .unwrap()
            .as_object()
            .cloned();
        let blocks = vec![
            agent_client_protocol::ContentBlock::Text(
                agent_client_protocol::TextContent::new("! ls -la").meta(meta),
            ),
            agent_client_protocol::ContentBlock::Text(agent_client_protocol::TextContent::new(
                "ignored",
            )),
        ];
        assert_eq!(
            Some("ls -la".to_owned()),
            PromptBlockMeta::command_in(&blocks)
        );
    }

    #[test]
    fn command_in_is_none_without_bash_meta() {
        let blocks = vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new("hello"),
        )];
        assert_eq!(None, PromptBlockMeta::command_in(&blocks));
    }

    #[test]
    fn command_in_skips_unrelated_meta() {
        let unrelated = serde_json::json!({"other": 1}).as_object().cloned();
        let bash = serde_json::to_value(PromptBlockMeta::bash("pwd"))
            .unwrap()
            .as_object()
            .cloned();
        let blocks = vec![
            agent_client_protocol::ContentBlock::Text(
                agent_client_protocol::TextContent::new("plain").meta(unrelated),
            ),
            agent_client_protocol::ContentBlock::Text(
                agent_client_protocol::TextContent::new("! pwd").meta(bash),
            ),
        ];
        assert_eq!(Some("pwd".to_owned()), PromptBlockMeta::command_in(&blocks));
    }
}
