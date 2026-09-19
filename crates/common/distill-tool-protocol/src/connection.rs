//! Connection-shape and tool-definition-mode enums.

use serde::{Deserialize, Serialize};

/// Role of a WebSocket connection. The computer hub uses this to decide
/// which methods are valid on a given socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    Harness,
    ToolServer,
    /// Grok-main / X-chat client of the bot-relay subsystem.
    BotClient,
}

impl ConnectionKind {
    pub const ALL: &'static [Self] = &[Self::Harness, Self::ToolServer, Self::BotClient];

    /// Wire string (`snake_case`), matching serde.
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::ToolServer => "tool_server",
            Self::BotClient => "bot_client",
        }
    }
}

impl std::fmt::Display for ConnectionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_wire_str_matches_serde_and_display_for_every_variant() {
        for kind in ConnectionKind::ALL {
            let serde_str = serde_json::to_value(kind)
                .expect("serialize")
                .as_str()
                .expect("string")
                .to_owned();
            assert_eq!(kind.as_wire_str(), serde_str);
            assert_eq!(kind.to_string(), serde_str);
            let back: ConnectionKind =
                serde_json::from_value(serde_json::Value::String(serde_str)).expect("deserialize");
            assert_eq!(back, *kind);
        }
    }
}

/// How the computer hub exposes the registered tool set to the model.
///
/// `Concise` carries a configurable meta-tool pair so callers can choose
/// the model-facing names of the search/invoke meta-tools per session.
///
/// Wire form is adjacently tagged on `mode`: `Full` serialises as
/// `{"mode": "full"}` (an object, not a bare string), and `Concise` as
/// `{"mode": "concise", "meta_search": "...", "meta_call": "..."}`.
///
/// `Copy` is intentionally NOT derived: `Concise`'s [`crate::ToolId`]
/// fields wrap heap strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolDefinitionMode {
    /// Every `ToolDescription` is sent to the model directly.
    Full,
    /// Only the meta-tool pair is sent; everything else is discoverable
    /// through the search meta-tool.
    Concise {
        /// Model-facing name of the search/discovery meta-tool.
        meta_search: crate::ToolId,
        /// Model-facing name of the call/invoke meta-tool.
        meta_call: crate::ToolId,
    },
}
