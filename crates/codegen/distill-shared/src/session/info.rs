use agent_client_protocol as acp;

/// Identifies a session by its `id` and `cwd`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Info {
    pub id: acp::SessionId,
    pub cwd: String,
}
