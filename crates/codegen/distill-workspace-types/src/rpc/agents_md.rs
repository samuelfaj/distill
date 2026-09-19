// Modified for Distill by Samuel Fajreldines, 2026.
//! Discovery method (`workspace.discover_agents_md`).

use serde::{Deserialize, Serialize};

use super::{RpcActivityClass, WorkspaceRpc};

/// `workspace.discover_agents_md` — project-instruction files (AGENTS.md /
/// Claude.md / `.grok/rules/*.md`) discovered from the workspace root up to
/// the git root, plus `~/.grok` and compat dirs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoverAgentsMdReq {}

impl WorkspaceRpc for DiscoverAgentsMdReq {
    const METHOD: &'static str = "workspace.discover_agents_md";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = Vec<AgentConfigFile>;
}

/// Mirrors the serde shape of `distill-agent`'s `AgentConfigFile`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigFile {
    pub file_name: String,
    pub file_path: String,
    pub content: String,
    /// Where discovery found the file; absent in payloads from servers that predate the field.
    #[serde(default)]
    pub source: InstructionSource,
}

/// Mirrors `distill-agent`'s `InstructionSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSource {
    /// `$GROK_HOME` and the vendor homes.
    Home,
    /// A `[paths] extra_rule_dirs` entry.
    Configured,
    /// The cwd-to-git-root chain.
    #[default]
    Project,
    /// A source this client predates; keeps the whole response deserializable when the server adds one.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_file_round_trips() {
        let raw = serde_json::json!({
            "file_name": "AGENTS.md",
            "file_path": "/repo/AGENTS.md",
            "content": "# Instructions\n",
            "source": "configured",
        });
        let file: AgentConfigFile = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(file.file_name, "AGENTS.md");
        assert_eq!(file.file_path, "/repo/AGENTS.md");
        assert_eq!(file.source, InstructionSource::Configured);
        assert_eq!(serde_json::to_value(&file).unwrap(), raw);
    }

    #[test]
    fn agent_config_file_ignores_unknown_fields() {
        let raw = serde_json::json!({
            "file_name": "Claude.md",
            "file_path": "/repo/Claude.md",
            "content": "x",
            "brand_new_field": {"nested": true},
        });
        let file: AgentConfigFile = serde_json::from_value(raw).unwrap();
        assert_eq!(file.file_name, "Claude.md");
        // Servers that predate `source` omit it; default is Project.
        assert_eq!(file.source, InstructionSource::Project);

        let raw = serde_json::json!({
            "file_name": "AGENTS.md",
            "file_path": "/repo/AGENTS.md",
            "content": "",
            "source": "some_future_source",
        });
        let file: AgentConfigFile = serde_json::from_value(raw).unwrap();
        assert_eq!(file.source, InstructionSource::Unknown);
    }
}
