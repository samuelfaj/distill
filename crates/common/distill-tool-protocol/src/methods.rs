//! Closed enumeration of every JSON-RPC method on the wire.
//!
//! Each variant is defined once in the [`define_methods!`] macro invocation
//! together with its wire string. The macro generates the enum, serde
//! renames, [`Method::as_wire_str`], and [`Method::from_wire_str`] from
//! that single source of truth.

use serde::{Deserialize, Serialize};

macro_rules! define_methods {
    (
        $(
            $(#[$var_attr:meta])*
            $variant:ident => $wire:literal
        ),* $(,)?
    ) => {
        /// Every JSON-RPC method understood by the computer hub.
        ///
        /// The variants are grouped by direction in source order; the enum is
        /// flat — direction enforcement is the computer hub's job, not the
        /// protocol crate's.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum Method {
            $(
                $(#[$var_attr])*
                #[serde(rename = $wire)]
                $variant,
            )*
        }

        impl Method {
            /// Every `Method` variant, for exhaustive iteration in tests.
            pub const ALL: &'static [Method] = &[$(Self::$variant,)*];

            /// Wire string for this method. Equivalent to the serde
            /// serialization but without a round-trip through `serde_json`.
            pub const fn as_wire_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)*
                }
            }

            /// Inverse of [`Self::as_wire_str`]. Returns `None` for
            /// strings that don't match any known method.
            pub fn from_wire_str(s: &str) -> Option<Self> {
                match s {
                    $($wire => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

/// Message prefix the hub uses when rejecting a request whose `method`
/// string does not parse into [`Method`] — the shape an OLD hub produces
/// for verbs it predates. Current clients answer hub skew from the
/// `hello_ack` `capabilities` advertisement instead of sniffing this
/// message, but the shape stays pinned here: terminal binaries built
/// while the SDK still keyed old-hub detection on this exact prefix
/// remain in the fleet. Do not change casually.
pub const UNKNOWN_METHOD_MSG_PREFIX: &str = "unknown method `";

define_methods! {
    // harness → service
    SessionOpen => "session_open",
    SessionClose => "session_close",
    /// The harness is leaving the session but the workspace is untouched,
    /// exactly as if the harness connection had dropped; the hub sweeps
    /// only what no other connection still holds. Used when a pooled
    /// connection stays open for other sessions. A separate verb (not a
    /// `session_close` flag) so a hub predating it rejects the frame with
    /// `-32601` instead of silently unbinding the workspace.
    SessionDetach => "session_detach",
    SessionBindServer => "session_bind_server",
    SessionUnbindServer => "session_unbind_server",
    /// Attach this harness connection to an EXISTING session as an
    /// observer. Answered hub-locally from the session→tool-server
    /// routing established by the owner's `session_bind_server` (or the
    /// server's re-`serve`); never forwarded to the tool server.
    SessionAttachServer => "session_attach_server",
    ToolsList => "tools.list",
    ToolsSearch => "tools.search",
    ToolCall => "tool.call",
    /// Sugar for [`Method::Hook`] with [`crate::HookEvent::Cancel`].
    /// SDKs translate this method to a hook frame before sending; there
    /// is no separate `tool.cancel` wire frame and no `ToolCancelParams`
    /// struct in [`crate::frames`].
    ToolCancel => "tool.cancel",
    ToolNotify => "tool.notify",
    SystemNotify => "system.notify",
    SubscribeNotifications => "subscribe_notifications",
    UnsubscribeNotifications => "unsubscribe_notifications",
    Hook => "hook",
    Hello => "hello",
    HelloAck => "hello_ack",
    Ping => "ping",
    Pong => "pong",

    // tool_server → service
    ToolCallProgress => "tool_call_progress",
    ToolNotification => "tool.notification",
    /// Reply to a request/response hook, correlated back to the harness by `hook_id`.
    HookReply => "hook_reply",
    /// Notification (no `id`, no response); rejects surface only in
    /// hub metrics. Only hub-minted trace-ids are accepted.
    TracesDonate => "traces.donate",
    /// Notification (no `id`, no response); rejects surface only in hub
    /// metrics. Donor service.name must be hub-allowlisted.
    LogsDonate => "logs.donate",
    /// Notification (no `id`, no response); rejects surface only in hub
    /// metrics. Donor service.name must be hub-allowlisted. No envelope
    /// `session_id` — metrics are process-aggregate.
    MetricsDonate => "metrics.donate",

    // service → tool_server
    ToolCallRequest => "tool_call_request",

    // service → harness
    ToolsChanged => "tools_changed",
    SubscribeAck => "subscribe_ack",
    UnsubscribeAck => "unsubscribe_ack",

    // harness → service (server discovery)
    /// List available tool servers for the authenticated user.
    ServersList => "servers.list",

    // tool_server status lifecycle
    ToolServerStatus => "tool_server.status",
    ToolServerGetStatus => "tool_server.get_status",
    ToolServerEvict => "tool_server.evict",

    // ── Session lifecycle ───────────────────────────────────────────

    /// Full tool snapshot for a session (server → hub). Idempotent:
    /// re-sending replaces the tool set; the hub diffs and emits
    /// `tools_changed`.
    Serve => "serve",
    /// Hub requests the server to start serving a session
    /// (hub → server). The server responds with its tool snapshot.
    SessionBind => "session.bind",
    /// Hub tells the server to stop serving a session
    /// (hub → server). Notification — no response expected.
    SessionUnbind => "session.unbind",

    // bot_client ↔ service (bot relay)
    /// Passthrough of an in-box gateway command. `name` and `args` are
    /// upstream-verbatim; `agentId` is hub routing metadata.
    BotCommand => "bot.command",
    /// Short-lived noVNC descriptor. May wake a hibernated box.
    BotVncDescriptor => "bot.vncDescriptor",
    /// Live agent roster read from the box. May wake a hibernated box; the
    /// hub bounds the wait and answers a retryable `box_unavailable`
    /// (`box_waking` / `box_hibernated`) or `box_migrating` while the box is
    /// coming up — never an empty list because of a failure.
    BotRoster => "bot.roster",
    /// Off-box run-state read. Cold — never wakes the box.
    BotStatus => "bot.status",
    /// Off-box transcript page. Cold — never wakes the box.
    BotTranscriptOffbox => "bot.transcript.offbox",
    /// Caller-scoped weekly Grok Bot usage summary. Cold — never wakes the
    /// box.
    BotUsage => "bot.usage",
    /// Subscribe this connection to `bot.event` for the given agents.
    BotSubscribe => "bot.subscribe",
    /// Drop this connection's `bot.event` subscription for the given agents.
    BotUnsubscribe => "bot.unsubscribe",
    /// Record a conversation → agents index. Does not route by conversation.
    BotBindConversation => "bot.bindConversation",
    /// Hub → client event notification (not a client-callable verb).
    BotEvent => "bot.event",
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_as_wire_str_from_wire_str() {
        for m in Method::ALL {
            assert_eq!(Method::from_wire_str(m.as_wire_str()), Some(*m));
        }
    }

    #[test]
    fn from_wire_str_returns_none_for_unknown() {
        assert_eq!(Method::from_wire_str("not_a_method"), None);
        assert_eq!(Method::from_wire_str(""), None);
    }

    #[test]
    fn snake_case_bot_method_aliases_are_rejected() {
        for alias in [
            "bot.vnc_descriptor",
            "bot.bind_conversation",
            "bot.link_status",
            "bot.linkStatus",
            "bot.transcript_offbox",
            "bot.ensure_box",
            "bot.ensureBox",
        ] {
            assert_eq!(Method::from_wire_str(alias), None, "{alias}");
            let err = serde_json::from_value::<Method>(serde_json::json!(alias)).unwrap_err();
            assert!(!err.to_string().is_empty(), "{alias}");
        }
    }
}
