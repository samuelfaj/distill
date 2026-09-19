//! The half of the "MCP servers that failed to connect" reminder that needs no `SessionActor`: classifying failures and rendering the section.
//! Episode dedupe lives in [`crate::session::announcement_state::McpAnnounced`]; the reminder is injected from `acp_session_impl/mcp.rs`.

use crate::session::announcement_state::{AnnouncedFailure, FailedServer};
use crate::session::managed_mcp::mcp_server_name;
use crate::util::text_sanitize::{flatten_spoofable, flatten_to_spaces};
use agent_client_protocol as acp;
use std::hash::{Hash, Hasher};

/// Hash of the config parts a user edits to fix a broken server (transport, url/command/args, header and env names, never header or env values).
/// Only ever compared for equality; hashing keeps fields that can carry credentials (URLs, arg values) out of the value itself.
/// Never persisted, so restored episodes adopt the current identity instead.
fn config_identity(cfg: &acp::McpServer) -> u64 {
    // Tuple/slice `Hash` impls delimit fields and prefix lengths, so empty items cannot collide across field boundaries
    fn hash_all(parts: impl Hash) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut hasher);
        hasher.finish()
    }
    fn header_names(headers: &[acp::HttpHeader]) -> Vec<&str> {
        headers.iter().map(|h| h.name.as_str()).collect()
    }
    match cfg {
        acp::McpServer::Http(c) => hash_all(("http", &c.url, header_names(&c.headers))),
        acp::McpServer::Sse(c) => hash_all(("sse", &c.url, header_names(&c.headers))),
        acp::McpServer::Stdio(c) => {
            let env_names: Vec<&str> = c.env.iter().map(|e| e.name.as_str()).collect();
            hash_all(("stdio", &c.command, &c.args, env_names))
        }
        // Future transports: fall back to the name (never Debug; an unknown variant's Debug output is unstable across versions)
        other => hash_all(("unknown", mcp_server_name(other))),
    }
}

/// Returns the failures to feed [`crate::session::announcement_state::McpAnnounced::note_failures`], sorted by name.
/// Skipped from the failure list (but kept in the unconnected set).
/// The skip while settling exists because the episode's one announcement should carry the real cause, not the "connection failed" placeholder.
pub(super) fn classify_failed_servers(
    mcp_state: &crate::session::mcp_servers::McpState,
    connected_names: &std::collections::HashSet<String>,
) -> (Vec<FailedServer>, std::collections::HashSet<String>) {
    let mut failed: Vec<FailedServer> = Vec::new();
    let mut unconnected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cfg in &mcp_state.configs {
        let name = mcp_server_name(cfg);
        if connected_names.contains(name) {
            continue;
        }
        unconnected.insert(name.to_string());
        if mcp_state.is_server_handshaking(name) {
            continue;
        }
        let is_auth = mcp_state.auth_required.contains(name);
        let has_record = is_auth || mcp_state.init_failed.contains_key(name);
        if !has_record && !mcp_state.is_initialized() {
            continue;
        }
        failed.push(FailedServer {
            name: name.to_string(),
            detail: mcp_state
                .init_failed
                .get(name)
                .filter(|d| !d.is_empty())
                .cloned(),
            class: if is_auth {
                AnnouncedFailure::AuthRequired
            } else {
                AnnouncedFailure::Transport
            },
            retries_on_use: !is_auth
                && matches!(cfg, acp::McpServer::Http(_) | acp::McpServer::Sse(_)),
            config_identity: config_identity(cfg),
        });
    }
    failed.sort_by(|a, b| a.name.cmp(&b.name));
    (failed, unconnected)
}

/// Render the "failed to connect" reminder section from the episodes being announced now.
/// Flattening stops them forging extra reminder lines or smuggling invisible/bidi characters.
/// A detail with nothing legible left falls back to the generic reason.
pub(super) fn render_failed_section(to_announce: &[FailedServer]) -> String {
    let mut s = "\nMCP servers that failed to connect:\n".to_string();
    for f in to_announce {
        let name = flatten_to_spaces(&f.name);
        let base = match f.class {
            AnnouncedFailure::AuthRequired => "auth required".to_string(),
            AnnouncedFailure::Transport => match f.detail.as_deref().and_then(flatten_spoofable) {
                // Quoted so printable remote text (e.g. an injected `) - other (…`) cannot pose as a sibling entry.
                // Embedded double quotes are downgraded to keep the delimiters unambiguous
                Some(detail) => format!("\"{}\"", detail.replace('"', "'")),
                None => "connection failed".to_string(),
            },
        };
        // The retry hint keeps the model using the server (a tool call re-handshakes it) instead of giving up, and giving up is self-fulfilling
        // Auth failures need user action
        let suffix = if f.retries_on_use {
            " — retries automatically on next tool call"
        } else {
            ""
        };
        s.push_str(&format!("- {name} ({base}{suffix})\n"));
    }
    s
}
