//! Client-presence attestation (`workspace.presence.note`).

use serde::{Deserialize, Serialize};

use super::{RpcActivityClass, WorkspaceRpc};

/// Canonical `ClientPresence` idle-withhold window.
/// The guest tracker uses it as its default and the gateway's refresh cadence must stay well inside it (pinned by a gateway test).
pub const PRESENCE_ACTIVITY_WINDOW_MS: u64 = 90_000;

/// Note client presence. Fire-and-forget. A visible note stamps `ClientPresence`; a hidden note stamps nothing so the window expires on its own.
/// A hide must never hibernate sooner than silence would.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceNoteReq {
    pub session_id: String,
    pub visible: bool,
    /// Strictly increasing per conversation. A note whose `seq` is not newer is dropped, so a slow visible note cannot restart a withhold after a newer hide.
    /// An absent `seq` (old gateway) always counts as newest.
    #[serde(default)]
    pub seq: Option<u64>,
}

impl WorkspaceRpc for PresenceNoteReq {
    const METHOD: &'static str = "workspace.presence.note";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = ();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_is_optional_on_the_wire() {
        let req: PresenceNoteReq =
            serde_json::from_str(r#"{"session_id":"s","visible":true}"#).unwrap();
        assert_eq!(None, req.seq);
    }
}
